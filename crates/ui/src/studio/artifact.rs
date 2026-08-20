//! Routed artifact viewer: full-bleed image strip, filmstrip, and inspector.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, DevicePixels, Element, GlobalElementId, Image,
    ImageFormat, InspectorElementId, IntoElement, LayoutId, ObjectFit, PinchEvent, Pixels, Point,
    Refineable, ScrollWheelEvent, SharedString, Style, StyleRefinement, Styled, TouchPhase, Window,
    canvas, div, point, prelude::*, px, size,
};
use zeron_proto::{StudioConversationView, StudioGalleryItem, StudioRunState};
use zeron_rpc::methods;
use zeron_studio::{MediaKind, StudioArtifactId, StudioConversationId, StudioRunId, StudioTurnId};

use crate::shader::shader;
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
/// Breathing room between the filmstrip's top edge and the video pill.
const ARTIFACT_FILMSTRIP_CLEARANCE: f32 = 12.0;
const ARTIFACT_ZOOM_MAX: f32 = 24.0;
/// Hard floor on the rubber-banded display zoom so the image cannot collapse.
const ARTIFACT_ZOOM_MIN: f32 = 0.55;
/// If macOS never sends Ended, settle the undershoot spring after this idle.
const ARTIFACT_ZOOM_IDLE: Duration = Duration::from_millis(160);
/// Slightly underdamped snap back to fit. ζ ≈ 0.84 at these values.
const ARTIFACT_ZOOM_SPRING_STIFFNESS: f32 = 320.0;
const ARTIFACT_ZOOM_SPRING_DAMPING: f32 = 30.0;
/// Full-size frames kept around the current filmstrip index.
const LIGHTBOX_PREFETCH: usize = 6;
const INSPECTOR_WIDTH: f32 = 320.0;
/// Matches the horizontal inset used by agent-chat rows in the left sidebar.
const INSPECTOR_PAD_X: f32 = Theme::SPACE_SM;
const INSPECTOR_COPY_SIZE: f32 = 24.0;
const INSPECTOR_COPY_GAP: f32 = 8.0;
/// Collapsed inspector prompt: ten rows of 12px sidebar type, then Show more.
const INSPECTOR_PROMPT_COLLAPSED_LINES: usize = 10;
const INSPECTOR_PROMPT_LINE_HEIGHT: f32 = 18.0;
/// Geist 12px Latin advance — scaled from the 14px chat-bubble estimate.
const INSPECTOR_PROMPT_ADVANCE: f32 = super::feed::PROMPT_AVG_CHAR_ADVANCE * (12.0 / 14.0);

/// Identity of a lightbox slot: a finished image, or an in-flight output.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) enum ArtifactFrameKey {
    Ready(StudioArtifactId),
    Loading {
        run_id: StudioRunId,
        output_ix: usize,
    },
}

impl ArtifactFrameKey {
    pub(super) fn artifact_id(self) -> Option<StudioArtifactId> {
        match self {
            Self::Ready(id) => Some(id),
            Self::Loading { .. } => None,
        }
    }
}

/// One frame the artifact viewer can show. Callers build the list;
/// the viewer never asks where it came from.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct ArtifactFrame {
    pub key: ArtifactFrameKey,
    pub conversation_id: StudioConversationId,
    pub turn_id: StudioTurnId,
    pub run_id: StudioRunId,
    pub output_ix: usize,
    pub prompt: String,
    pub model_display_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source_artifact_id: Option<StudioArtifactId>,
    pub state: StudioRunState,
    pub progress: Option<f32>,
    pub media_kind: MediaKind,
    pub duration_seconds: Option<f64>,
    pub error: Option<String>,
}

/// Provider message on a failed run. Empty strings are treated as missing so
/// the UI can fall back to a generic "Generation failed" label.
pub(super) fn run_error_message(error: Option<&str>) -> Option<&str> {
    error.map(str::trim).filter(|message| !message.is_empty())
}

/// Full-width wrapping chip used under a turn prompt and in the inspector.
pub(super) fn render_run_error_chip(theme: &Theme, message: &str) -> AnyElement {
    let red = theme.danger_muted;
    let danger = theme.danger;
    div()
        .w_full()
        .flex()
        .items_start()
        .gap(px(8.0))
        .rounded(px(10.0))
        .border_1()
        .border_color(danger.opacity(0.16))
        .bg(danger.opacity(0.05))
        .px(px(8.0))
        .py(px(7.0))
        .text_size(px(12.0))
        .child(
            div()
                .flex_none()
                .size(px(20.0))
                .rounded(px(6.0))
                .bg(danger.opacity(0.12))
                .flex()
                .items_center()
                .justify_center()
                .mt(px(1.0))
                .child(
                    crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                        .size(px(12.0))
                        .text_color(red.opacity(0.8)),
                ),
        )
        .child(
            div()
                .min_w_0()
                .flex_1()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .text_color(red.opacity(0.8))
                        .child(SharedString::from("Error")),
                )
                .child(
                    div()
                        .text_color(theme.text.opacity(0.8))
                        .child(SharedString::from(message.to_string())),
                ),
        )
        .into_any_element()
}

/// Centered overlay for a failed feed tile or lightbox stage.
pub(super) fn render_run_failed_overlay(theme: &Theme, error: Option<&str>) -> AnyElement {
    let red = theme.danger_muted;
    let danger = theme.danger;
    let message = run_error_message(error).map(|message| SharedString::from(message.to_string()));
    div()
        .size_full()
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .px(px(16.0))
        .gap(px(8.0))
        .child(
            div()
                .flex_none()
                .size(px(28.0))
                .rounded(px(8.0))
                .bg(danger.opacity(0.12))
                .flex()
                .items_center()
                .justify_center()
                .child(
                    crate::icons::icon(crate::icons::DANGER_TRIANGLE)
                        .size(px(14.0))
                        .text_color(red.opacity(0.8)),
                ),
        )
        .child(
            div()
                .text_size(px(12.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(red.opacity(0.9))
                .text_center()
                .child(SharedString::from("Generation failed")),
        )
        .when_some(message, |el, message| {
            el.child(
                div()
                    .text_size(px(11.0))
                    .text_color(theme.text.opacity(0.8))
                    .text_center()
                    .child(message),
            )
        })
        .into_any_element()
}

impl ArtifactFrame {
    pub(super) fn artifact_id(&self) -> Option<StudioArtifactId> {
        self.key.artifact_id()
    }

    pub(super) fn is_video(&self) -> bool {
        self.media_kind == MediaKind::Video
    }

    pub(super) fn is_loading(&self) -> bool {
        matches!(
            self.state,
            StudioRunState::Queued | StudioRunState::Running | StudioRunState::Downloading
        ) && self.artifact_id().is_none()
    }
}

fn run_duration_seconds(run: &zeron_proto::StudioRunView) -> Option<f64> {
    run.artifacts
        .iter()
        .find_map(|artifact| artifact.duration_seconds)
        .or_else(|| {
            run.controls.values().find_map(|value| match value {
                zeron_studio::ControlValue::DurationSeconds { value } => Some(*value),
                _ => None,
            })
        })
}

fn frame_accepts_run(run: &zeron_proto::StudioRunView) -> bool {
    matches!(run.model.output_kind, MediaKind::Image | MediaKind::Video)
}

fn frame_prompt(run: &zeron_proto::StudioRunView, turn: &zeron_proto::StudioTurnView) -> String {
    run.prompt
        .clone()
        .filter(|prompt| !prompt.is_empty())
        .unwrap_or_else(|| turn.prompt.clone())
}

/// The artifact route is re-applied on every shell render. A loading slot
/// has no artifact id, so the route still names the last Ready image —
/// re-selecting it would yank the viewer off the in-flight frame.
fn should_reapply_artifact_selection(
    selected: Option<ArtifactFrameKey>,
    requested: StudioArtifactId,
) -> bool {
    match selected {
        None => true,
        Some(ArtifactFrameKey::Ready(id)) => id != requested,
        Some(ArtifactFrameKey::Loading { .. }) => false,
    }
}

pub(super) fn resolve_frame_key(
    key: ArtifactFrameKey,
    frames: &[ArtifactFrame],
) -> Option<ArtifactFrameKey> {
    match key {
        ArtifactFrameKey::Ready(id) => frames
            .iter()
            .find(|frame| frame.artifact_id() == Some(id))
            .map(|frame| frame.key),
        ArtifactFrameKey::Loading { run_id, output_ix } => frames
            .iter()
            .find(|frame| frame.run_id == run_id && frame.output_ix == output_ix)
            .map(|frame| frame.key),
    }
}

pub(super) fn frames_from_gallery(items: &[StudioGalleryItem]) -> Vec<ArtifactFrame> {
    items
        .iter()
        .map(|item| ArtifactFrame {
            key: ArtifactFrameKey::Ready(item.id),
            conversation_id: item.conversation_id,
            turn_id: item.turn_id,
            run_id: StudioRunId::new(),
            output_ix: 0,
            prompt: item.prompt.clone(),
            model_display_name: item.model_display_name.clone(),
            mime_type: item.mime_type.clone(),
            size_bytes: item.size_bytes,
            width: item.width,
            height: item.height,
            source_artifact_id: item.source_artifact_id,
            state: StudioRunState::Succeeded,
            progress: None,
            media_kind: item.media_kind,
            duration_seconds: item.duration_seconds,
            error: None,
        })
        .collect()
}

pub(super) fn frames_from_conversation(view: &StudioConversationView) -> Vec<ArtifactFrame> {
    super::lineage::lineage_tiles(view)
        .into_iter()
        .filter_map(|tile| {
            let turn = view.turns.iter().find(|turn| turn.id == tile.turn_id)?;
            let run = turn.runs.iter().find(|run| run.id == tile.run_id)?;
            if !frame_accepts_run(run) {
                return None;
            }
            let prompt = frame_prompt(run, turn);
            let (aw, ah) = tile.aspect;
            if let Some(artifact_id) = tile.artifact_id {
                let artifact = run
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.id == artifact_id)?;
                if !matches!(artifact.media_kind, MediaKind::Image | MediaKind::Video) {
                    return None;
                }
                return Some(ArtifactFrame {
                    key: ArtifactFrameKey::Ready(artifact.id),
                    conversation_id: view.conversation.id,
                    turn_id: turn.id,
                    run_id: run.id,
                    output_ix: tile.output_ix,
                    prompt,
                    model_display_name: run.model.display_name.clone(),
                    mime_type: artifact.mime_type.clone(),
                    size_bytes: artifact.size_bytes,
                    width: artifact.width.or(Some(aw)),
                    height: artifact.height.or(Some(ah)),
                    source_artifact_id: tile.source_artifact_id,
                    state: run.state,
                    progress: run.progress,
                    media_kind: artifact.media_kind,
                    duration_seconds: artifact
                        .duration_seconds
                        .or_else(|| run_duration_seconds(run)),
                    error: run.error.clone(),
                });
            }
            if !matches!(
                run.state,
                StudioRunState::Queued
                    | StudioRunState::Running
                    | StudioRunState::Downloading
                    | StudioRunState::Failed
                    | StudioRunState::Cancelled
            ) {
                return None;
            }
            Some(ArtifactFrame {
                key: ArtifactFrameKey::Loading {
                    run_id: run.id,
                    output_ix: tile.output_ix,
                },
                conversation_id: view.conversation.id,
                turn_id: turn.id,
                run_id: run.id,
                output_ix: tile.output_ix,
                prompt,
                model_display_name: run.model.display_name.clone(),
                mime_type: String::new(),
                size_bytes: 0,
                width: (aw > 0).then_some(aw),
                height: (ah > 0).then_some(ah),
                source_artifact_id: tile.source_artifact_id,
                state: run.state,
                progress: run.progress,
                media_kind: tile.media_kind,
                duration_seconds: tile.duration_seconds.or_else(|| run_duration_seconds(run)),
                error: run.error.clone(),
            })
        })
        .collect()
}

pub(super) fn lightbox_neighbor_ids(
    frames: &[ArtifactFrame],
    selected: ArtifactFrameKey,
) -> Vec<StudioArtifactId> {
    let Some(index) = frames.iter().position(|frame| frame.key == selected) else {
        return Vec::new();
    };
    let mut ids = Vec::with_capacity(LIGHTBOX_PREFETCH * 2 + 1);
    if let Some(id) = frames[index].artifact_id() {
        ids.push(id);
    }
    for step in 1..=LIGHTBOX_PREFETCH {
        if let Some(id) = frames
            .get(index + step)
            .and_then(ArtifactFrame::artifact_id)
        {
            ids.push(id);
        }
        if let Some(id) = index
            .checked_sub(step)
            .and_then(|previous| frames.get(previous))
            .and_then(ArtifactFrame::artifact_id)
        {
            ids.push(id);
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

/// Bottom lift for the video control pill so it clears the filmstrip. The
/// pill rests at [`crate::video::CONTROLS_INSET`] above the video box, which
/// is vertically centered in the stage — lift only the deficit, and none at
/// all when the strip is hidden (edit mode) or the video already sits above
/// it.
fn filmstrip_controls_lift(filmstrip_visible: bool, stage_height: f32, video_height: f32) -> f32 {
    if !filmstrip_visible {
        return 0.0;
    }
    let bottom_gap = (stage_height - video_height).max(0.0) / 2.0;
    (ARTIFACT_FILMSTRIP_HEIGHT
        + ARTIFACT_FILMSTRIP_CLEARANCE
        - bottom_gap
        - crate::video::CONTROLS_INSET)
        .max(0.0)
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

/// Placeholder + sharp overlay for every Studio surface: gallery tiles,
/// thread tiles, filmstrip thumbs, and the lightbox. Fit is the only
/// layout difference — Cover in a square, Contain in a photo-sized box.
pub(super) fn cover_layers(
    base: Arc<Image>,
    overlay: Option<Arc<Image>>,
    radius: impl Into<Pixels>,
    fade: Option<SharedString>,
) -> AnyElement {
    layered_image(base, overlay, ObjectFit::Cover, radius.into(), fade)
}

pub(super) fn contain_layers(
    base: Arc<Image>,
    overlay: Option<Arc<Image>>,
    radius: impl Into<Pixels>,
    fade: Option<SharedString>,
) -> AnyElement {
    layered_image(base, overlay, ObjectFit::Contain, radius.into(), fade)
}

fn layered_image(
    base: Arc<Image>,
    overlay: Option<Arc<Image>>,
    fit: ObjectFit,
    radius: Pixels,
    fade: Option<SharedString>,
) -> AnyElement {
    let paint = |image: Arc<Image>| {
        let layer = match fit {
            ObjectFit::Cover => cover_image(image),
            _ => contain_image(image),
        }
        .size_full();
        if f32::from(radius) > 0.0 {
            layer.rounded(radius)
        } else {
            layer
        }
    };
    // Tiles need the clip so Cover/rounded corners don't spill. The lightbox
    // must not: overflow_hidden here hard-clips the photo above the filmstrip
    // and kills the dissolve under that chrome.
    let mut stack = div().size_full().relative();
    if f32::from(radius) > 0.0 {
        stack = stack.overflow_hidden().rounded(radius);
    }
    stack
        .child(paint(base))
        .when_some(overlay, |stack, overlay| {
            let layer = paint(overlay).absolute().inset_0();
            match fade {
                Some(id) => stack.child(motion::fade_quick(id, layer)),
                None => stack.child(layer),
            }
        })
        .into_any_element()
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

/// Spring that returns undershoot zoom and leftover pan to fit-to-stage.
#[derive(Debug, Clone, Copy)]
pub(super) struct LightboxZoomSpring {
    zoom_vel: f32,
    pan_x_vel: f32,
    pan_y_vel: f32,
    last_tick: Instant,
    /// `1` when fitting from above so the image cannot shrink past rest.
    zoom_floor: f32,
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

/// Inverse of [`rubber_band`].
fn unrubber_band(offset: f32, dimension: f32) -> f32 {
    let dimension = dimension.max(1.0);
    let y = offset.abs().min(dimension - 0.001);
    let x = y * dimension / (ARTIFACT_RUBBER_COEFF * (dimension - y));
    x.copysign(offset)
}

/// Displayed zoom → linear zoom, unbanding anything below fit.
fn logical_lightbox_zoom(displayed: f32) -> f32 {
    if displayed >= 1.0 {
        displayed
    } else {
        1.0 - unrubber_band(1.0 - displayed, 1.0)
    }
}

/// Linear zoom → displayed zoom, rubber-banding anything below fit.
fn display_lightbox_zoom(logical: f32) -> f32 {
    if logical >= 1.0 {
        logical.min(ARTIFACT_ZOOM_MAX)
    } else {
        (1.0 - rubber_band(1.0 - logical, 1.0)).max(ARTIFACT_ZOOM_MIN)
    }
}

fn apply_lightbox_zoom_factor(displayed: f32, factor: f32) -> f32 {
    let factor = if factor.is_finite() && factor > 0.0 {
        factor
    } else {
        1.0
    };
    display_lightbox_zoom(logical_lightbox_zoom(displayed.max(0.01)) * factor)
}

/// Keep `focus` (window coords) glued to the same screen point as zoom changes.
fn zoom_pan_around(
    zoom: f32,
    pan: Point<f32>,
    next_zoom: f32,
    focus: Point<f32>,
    stage_center: Point<f32>,
) -> Point<f32> {
    let old = zoom.max(0.01);
    let ratio = next_zoom / old;
    point(
        pan.x * ratio + (focus.x - stage_center.x) * (1.0 - ratio),
        pan.y * ratio + (focus.y - stage_center.y) * (1.0 - ratio),
    )
}

fn spring_toward(
    pos: f32,
    vel: f32,
    target: f32,
    dt: f32,
    stiffness: f32,
    damping: f32,
) -> (f32, f32) {
    let accel = stiffness * (target - pos) - damping * vel;
    let vel = vel + accel * dt;
    (pos + vel * dt, vel)
}

fn lightbox_viewer_transformed(zoom: f32, pan: Point<Pixels>) -> bool {
    (zoom - 1.0).abs() > 0.001 || f32::from(pan.x).abs() > 0.5 || f32::from(pan.y).abs() > 0.5
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

/// Size of a photo after ObjectFit::Contain into the stage.
fn lightbox_contain_size(stage_w: f32, stage_h: f32, image_w: f32, image_h: f32) -> (f32, f32) {
    if stage_w <= 1.0 || stage_h <= 1.0 || image_w <= 0.0 || image_h <= 0.0 {
        return (stage_w.max(0.0), stage_h.max(0.0));
    }
    let stage_aspect = stage_w / stage_h;
    let image_aspect = image_w / image_h;
    if image_aspect > stage_aspect {
        (stage_w, stage_w / image_aspect)
    } else {
        (stage_h * image_aspect, stage_h)
    }
}

/// Contained-image box inside the stage, including zoom, pan, and swipe.
pub(super) fn lightbox_image_paint_bounds(
    stage: Bounds<Pixels>,
    image_width: u32,
    image_height: u32,
    zoom: f32,
    pan: Point<Pixels>,
    swipe_x: f32,
) -> Bounds<Pixels> {
    let zoom = zoom.max(0.01);
    let zoomed_size = size(stage.size.width * zoom, stage.size.height * zoom);
    let zoomed = Bounds {
        origin: point(
            stage.origin.x + (stage.size.width - zoomed_size.width) / 2.0 + pan.x + px(swipe_x),
            stage.origin.y + (stage.size.height - zoomed_size.height) / 2.0 + pan.y,
        ),
        size: zoomed_size,
    };
    ObjectFit::Contain.get_bounds(
        zoomed,
        size(
            DevicePixels::from(image_width),
            DevicePixels::from(image_height),
        ),
    )
}

fn lightbox_click_hits_empty(
    stage: Bounds<Pixels>,
    image: Option<(u32, u32)>,
    zoom: f32,
    pan: Point<Pixels>,
    swipe_x: f32,
    click: Point<Pixels>,
) -> bool {
    let Some((width, height)) = image.filter(|(width, height)| *width > 0 && *height > 0) else {
        return true;
    };
    !lightbox_image_paint_bounds(stage, width, height, zoom, pan, swipe_x).contains(&click)
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

fn clipboard_image_from_bytes(mime: &str, bytes: Vec<u8>) -> Result<Image, String> {
    let format = ImageFormat::from_mime_type(mime)
        .or_else(|| zeron_studio::sniff_media_mime(&bytes).and_then(ImageFormat::from_mime_type))
        .ok_or_else(|| "unsupported image format".to_string())?;
    Ok(Image::from_bytes(format, bytes))
}

fn inspector_icon_action(
    id: &'static str,
    icon: &'static str,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(28.0))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(7.0))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.14)))
        .child(
            crate::icons::icon(icon)
                .size(px(16.0))
                .text_color(theme.text_muted.opacity(0.8)),
        )
}

/// Which sharp overlay a surface may GPU-upload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StudioPaint {
    /// Gallery, filmstrip, off-screen thread tiles: 512 preview only.
    Thumb,
    /// Visible thread tiles: 1280 display frame derived from the original.
    Display,
    /// Lightbox: native original.
    Full,
}

/// Minimum short edge retained for a gallery preview. The old longest-edge
/// thumbnail left only ~288 source pixels across the short edge of a common
/// 16:9 image, then enlarged that crop into a ~250px Retina tile.
const GALLERY_THUMB_SHORT_EDGE: u32 = 512;
/// Bound panoramas and unusually tall images without changing their aspect.
const GALLERY_THUMB_LONG_EDGE: u32 = 1536;
/// Thread tiles are ~400–520 CSS px; 1280 covers 2× Retina without keeping a
/// 4K RGBA bitmap (~30–64MB) on every cell.
const FEED_DISPLAY_SHORT_EDGE: u32 = 1280;
const FEED_DISPLAY_LONG_EDGE: u32 = 2048;

fn fit_image_dimensions(width: u32, height: u32, max_short: u32, max_long: u32) -> (u32, u32) {
    let short = width.min(height);
    let long = width.max(height);
    if short <= max_short && long <= max_long {
        return (width, height);
    }
    let scale_for_short = max_short as f64 / short.max(1) as f64;
    let scale_for_long = max_long as f64 / long.max(1) as f64;
    let scale = scale_for_short.min(scale_for_long).min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

fn gallery_thumb_dimensions(width: u32, height: u32) -> (u32, u32) {
    fit_image_dimensions(
        width,
        height,
        GALLERY_THUMB_SHORT_EDGE,
        GALLERY_THUMB_LONG_EDGE,
    )
}

fn feed_display_dimensions(width: u32, height: u32) -> (u32, u32) {
    fit_image_dimensions(
        width,
        height,
        FEED_DISPLAY_SHORT_EDGE,
        FEED_DISPLAY_LONG_EDGE,
    )
}

fn encode_jpeg_at(
    image: image::DynamicImage,
    width: u32,
    height: u32,
) -> Result<Arc<Image>, String> {
    let resized = if image.width() == width && image.height() == height {
        image
    } else {
        image.resize_exact(width, height, image::imageops::FilterType::Triangle)
    };
    let mut encoded = std::io::Cursor::new(Vec::new());
    resized
        .write_to(&mut encoded, image::ImageFormat::Jpeg)
        .map_err(|error| error.to_string())?;
    Ok(Arc::new(Image::from_bytes(
        ImageFormat::Jpeg,
        encoded.into_inner(),
    )))
}

pub(super) fn downsample_gallery_thumb(bytes: Vec<u8>) -> Result<Arc<Image>, String> {
    let image = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    // Preserve aspect because conversation tiles use Contain while gallery
    // and filmstrip tiles use Cover. Sizing from the short edge gives cover
    // crops a 2×+ pixel buffer without retaining the original frame.
    let (width, height) = gallery_thumb_dimensions(image.width(), image.height());
    encode_jpeg_at(image, width, height)
}

pub(super) fn downsample_feed_display(bytes: Vec<u8>) -> Result<Arc<Image>, String> {
    let image = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    let (width, height) = feed_display_dimensions(image.width(), image.height());
    encode_jpeg_at(image, width, height)
}

pub(super) async fn read_preview_bytes(
    engine: &EngineHandle,
    artifact_id: StudioArtifactId,
) -> Result<(String, String, Vec<u8>), String> {
    read_chunked_bytes(engine, methods::READ_STUDIO_PREVIEW_CHUNK, artifact_id).await
}

pub(super) async fn read_artifact_bytes(
    engine: &EngineHandle,
    artifact_id: StudioArtifactId,
) -> Result<(String, String, Vec<u8>), String> {
    read_chunked_bytes(engine, methods::READ_STUDIO_ARTIFACT_CHUNK, artifact_id).await
}

async fn read_chunked_bytes(
    engine: &EngineHandle,
    method: &str,
    artifact_id: StudioArtifactId,
) -> Result<(String, String, Vec<u8>), String> {
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    for _ in 0..4096 {
        let value = engine
            .client()
            .call(
                method,
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

    pub(super) fn copy_artifact_image(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        if let Some(image) = self.images.get_full(&artifact_id) {
            cx.write_to_clipboard(ClipboardItem::new_image(image.as_ref()));
            self.flash_copied_artifact(artifact_id, cx);
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = match read_artifact_bytes(&engine, artifact_id).await {
                Ok((_, mime, bytes)) => clipboard_image_from_bytes(&mime, bytes),
                Err(error) => Err(error),
            };
            this.update(cx, |page, cx| match result {
                Ok(image) => {
                    let image = Arc::new(image);
                    page.images.insert_full(artifact_id, image.clone());
                    cx.write_to_clipboard(ClipboardItem::new_image(image.as_ref()));
                    page.flash_copied_artifact(artifact_id, cx);
                }
                Err(error) => {
                    page.error = Some(error.into());
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    fn flash_copied_artifact(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        self.copied_artifact = Some(artifact_id);
        self.copied_artifact_clear = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1200))
                .await;
            this.update(cx, |page, cx| {
                page.copied_artifact = None;
                page.copied_artifact_clear = None;
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    pub(super) fn close_artifact_actions_menu(&mut self, cx: &mut Context<Self>) {
        if self.artifact_actions_menu.begin_close() {
            crate::popover::reap_popup(cx, |page: &mut Self| &mut page.artifact_actions_menu);
            cx.notify();
        }
    }

    pub(super) fn dismiss_artifact_actions_menu(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.keystroke.key == "escape" && self.artifact_actions_menu.is_open() {
            self.close_artifact_actions_menu(cx);
            true
        } else {
            false
        }
    }

    fn toggle_artifact_actions_menu(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let pressed_open = self.artifact_actions_menu.take_press_was_open();
        if pressed_open {
            self.close_artifact_actions_menu(cx);
        } else {
            self.close_image_menu(cx);
            self.close_upscale_settings_menu(cx);
            self.artifact_actions_menu.open(artifact_id);
            cx.notify();
        }
    }

    fn render_inspector_actions(
        &self,
        artifact_id: StudioArtifactId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let copied = self.copied_artifact == Some(artifact_id);
        let video = self.artifact_is_video(artifact_id);
        let menu_open = self.artifact_actions_menu.get() == Some(&artifact_id);
        let menu = menu_open.then(|| self.render_artifact_actions_menu(artifact_id, theme, cx));
        let copy_id = artifact_id;
        let download_id = artifact_id;
        let press_id = artifact_id;
        let toggle_id = artifact_id;

        let mut more =
            inspector_icon_action("studio-artifact-actions", crate::icons::MENU_DOTS, theme)
                .relative()
                .when(menu_open, |button| button.bg(crate::theme::wash(0.14)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |page, _, _, _| {
                        page.artifact_actions_menu
                            .note_trigger_press_matching(|id| id == &press_id);
                    }),
                )
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.toggle_artifact_actions_menu(toggle_id, cx);
                }));
        if let Some(menu) = menu {
            more = more.child(menu);
        }

        div()
            .mt(px(8.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_end()
            .gap(px(4.0))
            .when(video, |row| {
                row.child(
                    inspector_icon_action(
                        "studio-open-artifact-player",
                        crate::icons::EXPAND_ARROWS,
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.open_video_in_os_player(artifact_id, cx);
                    })),
                )
            })
            .when(!video, |row| {
                row.child(
                    inspector_icon_action(
                        "studio-copy-artifact",
                        if copied {
                            crate::icons::CHECK
                        } else {
                            crate::icons::COPY
                        },
                        theme,
                    )
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.copy_artifact_image(copy_id, cx);
                    })),
                )
            })
            .child(
                inspector_icon_action(
                    "studio-download-artifact",
                    crate::icons::DOWNLOAD_MINIMALISTIC,
                    theme,
                )
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.download_artifact(download_id, cx);
                })),
            )
            .child(more)
            .into_any_element()
    }

    fn render_artifact_actions_menu(
        &self,
        artifact_id: StudioArtifactId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let delete_id = artifact_id;
        let card = crate::popover::popover_card(theme)
            .w(px(170.0))
            .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                page.close_artifact_actions_menu(cx);
            }))
            .flex()
            .flex_col()
            .child(
                crate::popover::menu_row(
                    theme,
                    false,
                    format!("studio-artifact-menu-delete-{}", artifact_id.0),
                )
                .id("studio-artifact-menu-delete")
                .text_color(theme.danger)
                .on_click(cx.listener(move |page, _, _, cx| {
                    page.close_artifact_actions_menu(cx);
                    page.delete_artifact(delete_id, cx);
                }))
                .child(
                    crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                        .size(px(16.0))
                        .text_color(theme.danger),
                )
                .child(SharedString::from("Delete")),
            );
        crate::popover::anchored_menu_above_end(
            "studio-artifact-actions-menu",
            card.into_any_element(),
            self.artifact_actions_menu.closing_since(),
        )
    }

    pub(super) fn selected_artifact_id(&self) -> Option<StudioArtifactId> {
        self.selected_frame.and_then(ArtifactFrameKey::artifact_id)
    }

    pub(super) fn artifact_sequence(&self) -> Vec<ArtifactFrameKey> {
        self.lightbox_frames.iter().map(|frame| frame.key).collect()
    }

    pub(super) fn artifact_frame(&self, artifact_id: StudioArtifactId) -> Option<&ArtifactFrame> {
        self.lightbox_frames
            .iter()
            .find(|frame| frame.artifact_id() == Some(artifact_id))
    }

    pub(super) fn frame_by_key(&self, key: ArtifactFrameKey) -> Option<&ArtifactFrame> {
        self.lightbox_frames.iter().find(|frame| frame.key == key)
    }

    fn upscaled_source_for(&self, artifact_id: StudioArtifactId) -> Option<StudioArtifactId> {
        self.artifact_frame(artifact_id)
            .and_then(|frame| frame.source_artifact_id)
    }

    fn lightbox_display_artifact_id(&self) -> Option<StudioArtifactId> {
        let selected = self.selected_artifact_id()?;
        if self.compare_pressed {
            self.upscaled_source_for(selected).or(Some(selected))
        } else {
            Some(selected)
        }
    }

    pub(super) fn begin_artifact_compare(&mut self, cx: &mut Context<Self>) {
        let Some(selected) = self.selected_artifact_id() else {
            return;
        };
        let Some(source) = self.upscaled_source_for(selected) else {
            return;
        };
        self.compare_pressed = true;
        self.request_images(vec![source], true, cx);
        cx.stop_propagation();
        cx.notify();
    }

    pub(super) fn end_artifact_compare(&mut self, cx: &mut Context<Self>) {
        if self.compare_pressed {
            self.compare_pressed = false;
            cx.stop_propagation();
            cx.notify();
        }
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
        self.close_upscale_settings_menu(cx);
        self.close_artifact_actions_menu(cx);
        self.open_frame_viewer(ArtifactFrameKey::Ready(id), frames, cx);
    }

    pub(super) fn open_frame_viewer(
        &mut self,
        key: ArtifactFrameKey,
        frames: Vec<ArtifactFrame>,
        cx: &mut Context<Self>,
    ) {
        self.close_image_menu(cx);
        self.close_upscale_settings_menu(cx);
        self.close_artifact_actions_menu(cx);
        self.stop_hover_playback();
        self.lightbox_frames = frames;
        if !self.lightbox_frames.iter().any(|frame| frame.key == key) {
            let recovered = self.surface_artifact_frames();
            if recovered.iter().any(|frame| frame.key == key) {
                self.lightbox_frames = recovered;
            }
        }
        if let Some(index) = self
            .lightbox_frames
            .iter()
            .position(|frame| frame.key == key)
        {
            self.select_artifact_index(index, cx);
            return;
        }
        if self.selected_frame.take().is_some() {
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

    fn reset_lightbox_zoom_motion(&mut self) {
        self.lightbox_zoom_spring = None;
        self.lightbox_zoom_last_tick = None;
    }

    fn snap_lightbox_to_fit(&mut self) {
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        self.reset_lightbox_zoom_motion();
    }

    pub(super) fn lightbox_motion_pending(&self) -> bool {
        self.lightbox_zoom_spring.is_some()
            || (self.lightbox_zoom_should_settle()
                && self
                    .lightbox_zoom_last_tick
                    .is_some_and(|last| last.elapsed() >= ARTIFACT_ZOOM_IDLE))
            || self.lightbox_snap.is_some()
            || (self.lightbox_swipe_x.abs() > 0.5
                && self
                    .lightbox_swipe_last_tick
                    .is_some_and(|last| last.elapsed() >= ARTIFACT_SWIPE_IDLE))
    }

    pub(super) fn reset_lightbox_viewer(&mut self) {
        self.compare_pressed = false;
        self.snap_lightbox_to_fit();
        self.lightbox_drag = None;
        self.reset_lightbox_swipe();
        self.lightbox_swipe_locked = false;
        self.stop_video_playback();
    }

    pub(super) fn adopt_artifact_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let sequence = self.artifact_sequence();
        let Some(key) = sequence.get(index).copied() else {
            return false;
        };
        let changed = self.selected_frame != Some(key);
        self.selected_frame = Some(key);
        self.compare_pressed = false;
        self.snap_lightbox_to_fit();
        self.lightbox_drag = None;
        if changed {
            self.sync_video_playback(cx);
        }
        if changed {
            self.close_upscale_settings_menu(cx);
            self.close_artifact_actions_menu(cx);
            self.inspector_scroll.set_offset(Point::default());
        }
        if let Some(artifact_id) = key.artifact_id()
            && let Some(conversation_id) = self.artifact_conversation(artifact_id)
        {
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
        let Some(selected) = self.selected_frame else {
            return Vec::new();
        };
        let sequence = self.artifact_sequence();
        let Some(index) = sequence.iter().position(|key| *key == selected) else {
            return Vec::new();
        };
        let range = filmstrip_visible_range(
            index,
            sequence.len(),
            filmstrip_viewport_width(self.lightbox_stage_width),
        );
        sequence
            .get(range)
            .unwrap_or(&[])
            .iter()
            .filter_map(|key| key.artifact_id())
            .collect()
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
        let Some(selected) = self.selected_frame else {
            return false;
        };
        let Some(index) = artifacts.iter().position(|key| *key == selected) else {
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
            if should_reapply_artifact_selection(self.selected_frame, artifact_id) {
                if let Some(index) = self
                    .lightbox_frames
                    .iter()
                    .position(|frame| frame.artifact_id() == Some(artifact_id))
                {
                    self.adopt_artifact_index(index, cx);
                }
            }
            return;
        }
        let frames = self.surface_artifact_frames();
        if frames
            .iter()
            .any(|frame| frame.artifact_id() == Some(artifact_id))
        {
            self.open_artifact_viewer(artifact_id, frames, cx);
            return;
        }
        if self.selected_conversation != Some(conversation_id) {
            self.open_conversation(conversation_id, cx);
        }
    }

    pub fn close_artifact(&mut self, cx: &mut Context<Self>) {
        self.close_image_menu(cx);
        self.close_upscale_settings_menu(cx);
        self.close_artifact_actions_menu(cx);
        self.stop_video_playback();
        let previous = self.selected_artifact_id();
        if self.selected_frame.take().is_some() {
            if let Some(id) = previous {
                self.reveal_gallery_artifact_if_needed(id);
            }
            self.lightbox_frames.clear();
            self.reset_lightbox_viewer();
            self.request_visible_gallery_images(cx);
            cx.notify();
        }
    }

    /// Close via chrome, empty-space click, or Escape. Emits so the shell
    /// pops the artifact route; [`close_artifact`] then clears viewer state.
    pub(super) fn request_close_artifact(&mut self, cx: &mut Context<Self>) {
        self.close_upscale_settings_menu(cx);
        self.close_artifact_actions_menu(cx);
        self.exit_edit_mode(cx);
        self.stop_video_playback();
        let previous = self.selected_artifact_id();
        if self.selected_frame.take().is_some() {
            if let Some(id) = previous {
                self.reveal_gallery_artifact_if_needed(id);
            }
            cx.emit(StudioEvent::CloseArtifact);
            cx.notify();
        }
    }

    fn lightbox_stage_bounds(&self) -> Bounds<Pixels> {
        Bounds {
            origin: self.lightbox_stage_origin,
            size: size(
                px(self.lightbox_stage_width),
                px(self.lightbox_stage_height),
            ),
        }
    }

    fn lightbox_image_pixel_size(&self, window: &mut Window, cx: &mut App) -> Option<(u32, u32)> {
        let id = self.lightbox_display_artifact_id()?;
        self.photo_pixel_size(id, window, cx)
    }

    /// Real photo size for a slide. Never the ThumbHash decode — that is 5:7
    /// for a 2:3 image and would recreate the side-bar halo.
    pub(super) fn photo_pixel_size(
        &self,
        id: StudioArtifactId,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<(u32, u32)> {
        if let Some(size) = self.artifact_pixel_size(id) {
            return Some(size);
        }
        let image = self
            .images
            .get_full(&id)
            .or_else(|| self.images.get_display(&id))
            .or_else(|| self.images.get_thumb_only(&id))?;
        let data = image.use_render_image(window, cx)?;
        if data.frame_count() == 0 {
            return None;
        }
        let size = data.size(0);
        let width = u32::from(size.width);
        let height = u32::from(size.height);
        (width > 0 && height > 0).then_some((width, height))
    }

    fn lightbox_click_is_empty(
        &self,
        click: Point<Pixels>,
        window: &mut Window,
        cx: &mut App,
    ) -> bool {
        if self.lightbox_stage_width <= 1.0 || self.lightbox_stage_height <= 1.0 {
            return true;
        }
        lightbox_click_hits_empty(
            self.lightbox_stage_bounds(),
            self.lightbox_image_pixel_size(window, cx),
            self.lightbox_zoom,
            self.lightbox_pan,
            self.lightbox_swipe_x,
            click,
        )
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
        if self.lightbox_zoom <= 1.001 {
            return;
        }
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

    fn lightbox_stage_center(&self) -> Point<f32> {
        let (width, height) = self.lightbox_stage_size();
        point(
            f32::from(self.lightbox_stage_origin.x) + width / 2.0,
            f32::from(self.lightbox_stage_origin.y) + height / 2.0,
        )
    }

    fn lightbox_viewer_is_transformed(&self) -> bool {
        self.lightbox_zoom_spring.is_some()
            || lightbox_viewer_transformed(self.lightbox_zoom, self.lightbox_pan)
    }

    fn lightbox_zoom_should_settle(&self) -> bool {
        self.lightbox_zoom < 1.001
            && lightbox_viewer_transformed(self.lightbox_zoom, self.lightbox_pan)
    }

    pub(super) fn adjust_lightbox_zoom(
        &mut self,
        factor: f32,
        focus: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        self.lightbox_zoom_spring = None;
        self.lightbox_zoom_last_tick = Some(Instant::now());
        let previous = self.lightbox_zoom.max(0.01);
        let next = apply_lightbox_zoom_factor(previous, factor);
        if let Some(focus) = focus {
            let pan = zoom_pan_around(
                previous,
                point(
                    f32::from(self.lightbox_pan.x),
                    f32::from(self.lightbox_pan.y),
                ),
                next,
                point(f32::from(focus.x), f32::from(focus.y)),
                self.lightbox_stage_center(),
            );
            self.lightbox_pan = point(px(pan.x), px(pan.y));
        }
        self.lightbox_zoom = next;
        if self.lightbox_viewer_is_transformed() {
            self.reset_lightbox_swipe();
        }
        self.clamp_lightbox_pan();
        cx.notify();
    }

    pub(super) fn fit_lightbox(&mut self, cx: &mut Context<Self>) {
        self.start_lightbox_zoom_spring(cx);
    }

    fn settle_lightbox_zoom(&mut self, cx: &mut Context<Self>) {
        if self.lightbox_zoom_should_settle() {
            self.start_lightbox_zoom_spring(cx);
        }
    }

    fn start_lightbox_zoom_spring(&mut self, cx: &mut Context<Self>) {
        self.lightbox_zoom_last_tick = None;
        if !lightbox_viewer_transformed(self.lightbox_zoom, self.lightbox_pan) {
            self.snap_lightbox_to_fit();
            return;
        }
        if crate::motion::reduced_motion(cx) {
            self.snap_lightbox_to_fit();
            cx.notify();
            return;
        }
        if self.lightbox_zoom_spring.is_none() {
            self.lightbox_zoom_spring = Some(LightboxZoomSpring {
                zoom_vel: 0.0,
                pan_x_vel: 0.0,
                pan_y_vel: 0.0,
                last_tick: Instant::now(),
                zoom_floor: if self.lightbox_zoom >= 1.0 {
                    1.0
                } else {
                    ARTIFACT_ZOOM_MIN
                },
            });
        }
        cx.notify();
    }

    fn step_lightbox_zoom_spring(&mut self, cx: &mut Context<Self>) {
        let Some(mut spring) = self.lightbox_zoom_spring else {
            return;
        };
        let dt = spring
            .last_tick
            .elapsed()
            .as_secs_f32()
            .clamp(1.0 / 240.0, 1.0 / 20.0)
            / motion::speed_scale();
        spring.last_tick = Instant::now();
        let (mut zoom, mut zoom_vel) = spring_toward(
            self.lightbox_zoom,
            spring.zoom_vel,
            1.0,
            dt,
            ARTIFACT_ZOOM_SPRING_STIFFNESS,
            ARTIFACT_ZOOM_SPRING_DAMPING,
        );
        let (pan_x, pan_x_vel) = spring_toward(
            f32::from(self.lightbox_pan.x),
            spring.pan_x_vel,
            0.0,
            dt,
            ARTIFACT_ZOOM_SPRING_STIFFNESS,
            ARTIFACT_ZOOM_SPRING_DAMPING,
        );
        let (pan_y, pan_y_vel) = spring_toward(
            f32::from(self.lightbox_pan.y),
            spring.pan_y_vel,
            0.0,
            dt,
            ARTIFACT_ZOOM_SPRING_STIFFNESS,
            ARTIFACT_ZOOM_SPRING_DAMPING,
        );
        let settled = (zoom - 1.0).abs() < 0.0015
            && zoom_vel.abs() < 0.08
            && pan_x.abs() < 0.5
            && pan_y.abs() < 0.5
            && pan_x_vel.abs() < 8.0
            && pan_y_vel.abs() < 8.0;
        if settled {
            self.snap_lightbox_to_fit();
            cx.notify();
            return;
        }
        if zoom < spring.zoom_floor {
            zoom = spring.zoom_floor;
            zoom_vel = 0.0;
        } else {
            zoom = zoom.max(ARTIFACT_ZOOM_MIN);
        }
        self.lightbox_zoom = zoom;
        self.lightbox_pan = point(px(pan_x), px(pan_y));
        spring.zoom_vel = zoom_vel;
        spring.pan_x_vel = pan_x_vel;
        spring.pan_y_vel = pan_y_vel;
        self.lightbox_zoom_spring = Some(spring);
        cx.notify();
    }

    pub(super) fn begin_lightbox_pan(
        &mut self,
        position: Point<Pixels>,
        modifiers: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        if self.edit_target.is_some() && self.begin_edit_stroke(position, modifiers, cx) {
            return;
        }
        if self.lightbox_viewer_is_transformed() {
            self.lightbox_zoom_spring = None;
            self.lightbox_drag = Some(position);
            cx.notify();
        }
    }

    pub(super) fn update_lightbox_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self
            .edit_paint
            .as_ref()
            .is_some_and(super::paint::PaintSession::is_drawing)
        {
            self.extend_edit_stroke(position, cx);
            return;
        }
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
        if self
            .edit_paint
            .as_ref()
            .is_some_and(super::paint::PaintSession::is_drawing)
        {
            self.end_edit_stroke(cx);
            return;
        }
        if self.lightbox_drag.take().is_some() {
            if self.lightbox_zoom_should_settle() {
                self.settle_lightbox_zoom(cx);
            } else {
                cx.notify();
            }
        }
    }

    pub(super) fn finish_lightbox_snap_immediate(&mut self, cx: &mut Context<Self>) {
        if self.lightbox_zoom_spring.is_some() || self.lightbox_zoom_should_settle() {
            self.snap_lightbox_to_fit();
        }
        let target = self.lightbox_snap.map(|snap| snap.to).unwrap_or(0.0);
        self.commit_lightbox_snap_target(target, cx);
    }

    fn commit_lightbox_snap_target(&mut self, target: f32, cx: &mut Context<Self>) {
        if target.abs() > 1.0 {
            let artifacts = self.artifact_sequence();
            let index = self
                .selected_frame
                .and_then(|selected| artifacts.iter().position(|key| *key == selected))
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
        if self.lightbox_zoom_spring.is_some() {
            self.step_lightbox_zoom_spring(cx);
            return;
        }
        if self.lightbox_zoom_should_settle()
            && self
                .lightbox_zoom_last_tick
                .is_some_and(|last| last.elapsed() >= ARTIFACT_ZOOM_IDLE)
        {
            self.settle_lightbox_zoom(cx);
            return;
        }
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
            .selected_frame
            .and_then(|selected| artifacts.iter().position(|key| *key == selected))
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
        if event.modifiers.platform && !self.selected_is_video() {
            let movement = if vertical.abs() >= horizontal.abs() {
                vertical
            } else {
                horizontal
            };
            if movement.abs() > f32::EPSILON {
                self.adjust_lightbox_zoom((movement * 0.01).exp(), Some(event.position), cx);
            }
            if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.settle_lightbox_zoom(cx);
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
        if self.lightbox_viewer_is_transformed() {
            if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
                self.settle_lightbox_zoom(cx);
            } else {
                self.pan_lightbox(horizontal, vertical, cx);
                if self.lightbox_zoom_should_settle() {
                    self.lightbox_zoom_last_tick = Some(Instant::now());
                }
            }
            cx.stop_propagation();
            return;
        }
        if self.edit_target.is_some() {
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
            .selected_frame
            .and_then(|selected| artifacts.iter().position(|key| *key == selected))
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
        self.lightbox_zoom_spring = None;
        self.lightbox_pan.x = px(f32::from(self.lightbox_pan.x) + dx);
        self.lightbox_pan.y = px(f32::from(self.lightbox_pan.y) + dy);
        self.clamp_lightbox_pan();
        cx.notify();
    }

    pub(super) fn on_lightbox_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        if self.selected_is_video() {
            cx.stop_propagation();
            return;
        }
        if matches!(event.phase, TouchPhase::Started) {
            self.lightbox_zoom_spring = None;
        }
        if event.delta.abs() > f32::EPSILON {
            self.adjust_lightbox_zoom((1.0 + event.delta).max(0.05), Some(event.position), cx);
        }
        if matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.settle_lightbox_zoom(cx);
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

    /// Layers for a tile or lightbox slide.
    ///
    /// The thumbhash (or 512 preview) stays mounted as the base. A sharper
    /// overlay is added only after `use_render_image` has a GPU tile, so
    /// promoting never paints an empty frame.
    pub(super) fn display_layers(
        &self,
        id: StudioArtifactId,
        paint: StudioPaint,
        window: &mut Window,
        cx: &mut gpui::App,
    ) -> (Option<Arc<Image>>, Option<Arc<Image>>) {
        let placeholder = self.images.get_placeholder(&id);
        let thumb = self.images.get_thumb_only(&id);
        let sharp = match paint {
            StudioPaint::Thumb => None,
            StudioPaint::Display => self.images.get_display(&id),
            StudioPaint::Full => self.images.get_full(&id),
        };

        if let Some(placeholder) = placeholder {
            let _ = placeholder.clone().use_render_image(window, cx);
            let overlay = match &sharp {
                Some(image) if image.clone().use_render_image(window, cx).is_some() => {
                    Some(image.clone())
                }
                _ => match &thumb {
                    Some(image) if image.clone().use_render_image(window, cx).is_some() => {
                        Some(image.clone())
                    }
                    _ => None,
                },
            };
            return (Some(placeholder), overlay);
        }

        let base = match paint {
            StudioPaint::Full => thumb
                .clone()
                .or_else(|| self.images.get_thumb(&id))
                .or_else(|| sharp.clone()),
            StudioPaint::Display => thumb.clone().or_else(|| sharp.clone()),
            StudioPaint::Thumb => thumb.clone(),
        };
        if let Some(base) = base.as_ref() {
            let _ = base.clone().use_render_image(window, cx);
        }
        let overlay = match &sharp {
            Some(image)
                if !base.as_ref().is_some_and(|base| Arc::ptr_eq(base, image))
                    && image.clone().use_render_image(window, cx).is_some() =>
            {
                Some(image.clone())
            }
            _ => None,
        };
        (base, overlay)
    }

    fn warm_lightbox_neighbors(&self, window: &mut Window, cx: &mut gpui::App) {
        let Some(selected) = self.selected_frame else {
            return;
        };
        // GPU-warm only the current slide and its immediate neighbors. Encoded
        // prefetch still walks LIGHTBOX_PREFETCH; uploading all of those
        // originals is what pushed Activity Monitor past 4GB.
        let mut ids = Vec::with_capacity(LIGHTBOX_PREFETCH * 2 + 2);
        if let Some(ready) = selected.artifact_id()
            && let Some(source) = self.upscaled_source_for(ready)
        {
            ids.push(source);
        }
        ids.extend(lightbox_neighbor_ids(&self.lightbox_frames, selected));
        for id in ids.into_iter().take(4) {
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
        key: Option<ArtifactFrameKey>,
        page: Option<(f32, f32)>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let artifact_id = key.and_then(ArtifactFrameKey::artifact_id);
        let slot = key.and_then(|key| self.frame_by_key(key));
        let video = slot.is_some_and(ArtifactFrame::is_video);
        let (base, overlay) = artifact_id
            .map(|id| self.display_layers(id, StudioPaint::Full, window, cx))
            .unwrap_or((None, None));
        let slide_id = match key {
            Some(ArtifactFrameKey::Ready(id)) => {
                SharedString::from(format!("studio-artifact-slide-{}", id.0))
            }
            Some(ArtifactFrameKey::Loading { run_id, output_ix }) => {
                SharedString::from(format!("studio-artifact-slide-{}-{output_ix}", run_id.0))
            }
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
        let transformed = page.is_none()
            && (self.lightbox_zoom_spring.is_some()
                || lightbox_viewer_transformed(zoom, self.lightbox_pan));
        if video {
            return self.render_video_slide(frame, slot, base, overlay, cx);
        }
        let image_size = artifact_id.and_then(|id| self.photo_pixel_size(id, window, cx));
        let stack = |base: Arc<Image>, overlay: Option<Arc<Image>>| {
            // Once the sharp frame has a GPU tile, drop the ThumbHash. A 2:3
            // hash decodes as 5:7; stacked in the full stage it shows as
            // blurred side bars. Size a placeholder-only slide to the photo
            // box (no overflow_hidden — that clip cuts the filmstrip fade).
            let placeholder_only = overlay.is_none();
            let paint = overlay.unwrap_or(base);
            let mut layer = div().relative();
            let zoom = zoom.max(0.01);
            layer = if placeholder_only && measured {
                if let Some((image_w, image_h)) = image_size {
                    let (width, height) = lightbox_contain_size(
                        self.lightbox_stage_width,
                        self.lightbox_stage_height,
                        image_w as f32,
                        image_h as f32,
                    );
                    let scale = if transformed { zoom } else { 1.0 };
                    let layer = layer.flex_none().w(px(width * scale)).h(px(height * scale));
                    if transformed {
                        layer.left(self.lightbox_pan.x).top(self.lightbox_pan.y)
                    } else {
                        layer
                    }
                } else if !transformed {
                    layer.size_full()
                } else {
                    layer
                        .flex_none()
                        .w(px(self.lightbox_stage_width * zoom))
                        .h(px(self.lightbox_stage_height * zoom))
                        .left(self.lightbox_pan.x)
                        .top(self.lightbox_pan.y)
                }
            } else if !transformed {
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
            layer.child(contain_layers(paint, None, px(0.0), None))
        };
        match base {
            Some(base) => frame.child(stack(base, overlay)).into_any_element(),
            None => {
                let loading = slot.and_then(|slot| {
                    let seed = slot.run_id.0.as_u128() as u32 ^ slot.output_ix as u32;
                    let (effect, wash) = super::feed::loading_effect(
                        seed,
                        slot.output_ix as u32 % 4,
                        slot.state,
                        slot.progress,
                    )?;
                    let (aw, ah) = slot
                        .width
                        .zip(slot.height)
                        .filter(|(w, h)| *w > 0 && *h > 0)
                        .unwrap_or((1, 1));
                    let (width, height) = if measured {
                        lightbox_contain_size(
                            self.lightbox_stage_width,
                            self.lightbox_stage_height,
                            aw as f32,
                            ah as f32,
                        )
                    } else {
                        (320.0, 320.0 * ah as f32 / aw.max(1) as f32)
                    };
                    Some(
                        shader(effect)
                            .progress(wash)
                            .flex_none()
                            .w(px(width))
                            .h(px(height))
                            .rounded(px(12.0)),
                    )
                });
                let failed = slot.is_some_and(|slot| slot.state == StudioRunState::Failed);
                let show_fallback = loading.is_none() && !failed;
                frame
                    .child(
                        div()
                            .relative()
                            .flex()
                            .items_center()
                            .justify_center()
                            .when_some(loading, |box_, fill| box_.child(fill))
                            .when(failed, |box_| {
                                let error = slot.and_then(|slot| slot.error.as_deref());
                                box_.size_full()
                                    .child(render_run_failed_overlay(theme, error))
                            })
                            .when(show_fallback, |box_| {
                                box_.text_size(px(12.0))
                                    .text_color(theme.text_faint)
                                    .child("Loading image…")
                            }),
                    )
                    .into_any_element()
            }
        }
    }

    fn video_stage_size(&self, slot: Option<&ArtifactFrame>) -> (f32, f32) {
        let (aw, ah) = slot
            .and_then(|slot| slot.width.zip(slot.height))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or((16, 9));
        if self.lightbox_stage_width > 1.0 && self.lightbox_stage_height > 1.0 {
            lightbox_contain_size(
                self.lightbox_stage_width,
                self.lightbox_stage_height,
                aw as f32,
                ah as f32,
            )
        } else {
            let height = 360.0;
            (height * aw as f32 / ah.max(1) as f32, height)
        }
    }

    fn render_video_slide(
        &self,
        frame: gpui::Stateful<gpui::Div>,
        slot: Option<&ArtifactFrame>,
        base: Option<Arc<Image>>,
        overlay: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (width, height) = self.video_stage_size(slot);
        let mut stage = div()
            .relative()
            .flex_none()
            .w(px(width))
            .h(px(height))
            .overflow_hidden()
            .rounded(px(12.0))
            .bg(crate::theme::ink(0.04));
        if slot.is_some_and(|slot| slot.state == StudioRunState::Failed) {
            let theme = Theme::of(cx);
            let error = slot.and_then(|slot| slot.error.as_deref());
            return frame
                .child(stage.child(render_run_failed_overlay(&theme, error)))
                .into_any_element();
        }
        if let Some(base) = base {
            stage = stage.child(contain_layers(base, overlay, px(0.0), None));
        }
        stage = stage.child(self.render_video_stage_overlay(slot, cx));
        frame.child(stage).into_any_element()
    }

    fn render_video_stage_overlay(
        &self,
        slot: Option<&ArtifactFrame>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let chrome = crate::video::VideoChrome {
            playing: self.video.as_ref().is_some_and(|player| player.playing),
            muted: self.video.as_ref().is_some_and(|player| player.muted),
            loading: self.video.as_ref().is_some_and(|player| player.loading),
            position: self
                .video
                .as_ref()
                .map(|player| player.position)
                .unwrap_or(0.0),
            duration: self
                .video
                .as_ref()
                .and_then(|player| player.duration)
                .or_else(|| slot.and_then(|slot| slot.duration_seconds)),
        };
        let entity = cx.weak_entity();
        let controls_inset = gpui::Edges {
            bottom: filmstrip_controls_lift(
                self.edit_target.is_none(),
                self.lightbox_stage_height,
                self.video_stage_size(slot).1,
            ),
            ..Default::default()
        };
        let player = crate::video::player("studio-video", chrome)
            .controls_inset(controls_inset)
            .on_toggle_play({
                let entity = entity.clone();
                move |_, cx| {
                    let _ = entity.update(cx, |page, cx| page.toggle_selected_video(cx));
                }
            })
            .on_toggle_mute({
                let entity = entity.clone();
                move |_, cx| {
                    let _ = entity.update(cx, |page, cx| page.toggle_selected_video_mute(cx));
                }
            })
            .on_seek({
                let entity = entity.clone();
                move |seconds, _, cx| {
                    let _ = entity.update(cx, |page, cx| page.seek_selected_video(seconds, cx));
                }
            });
        #[cfg(target_os = "macos")]
        let player = {
            if let Some(buffer) = self
                .video
                .as_ref()
                .and_then(super::video::StudioVideoPlayback::frame)
            {
                player.child(
                    gpui::surface(buffer)
                        .object_fit(gpui::ObjectFit::Contain)
                        .size_full()
                        .absolute()
                        .inset_0(),
                )
            } else {
                player
            }
        };
        div()
            .absolute()
            .inset_0()
            .size_full()
            .child(player)
            .into_any_element()
    }

    pub(super) fn render_artifact_page(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let key = self.selected_frame?;
        let id = key.artifact_id();
        self.warm_lightbox_neighbors(window, cx);
        let sequence = self.artifact_sequence();
        let selected_index = sequence
            .iter()
            .position(|candidate| *candidate == key)
            .unwrap_or(0);
        let selected = self.frame_by_key(key);
        let details = selected.map(|frame| {
            (
                frame.turn_id,
                frame.prompt.clone(),
                frame.model_display_name.clone(),
                frame.mime_type.clone(),
                frame.size_bytes,
                frame.is_loading() || frame.artifact_id().is_none(),
                frame.duration_seconds,
                frame.is_video(),
            )
        });
        let failed_error = selected
            .filter(|frame| frame.state == StudioRunState::Failed)
            .and_then(|frame| run_error_message(frame.error.as_deref()).map(str::to_string));
        let compare_source = id.and_then(|id| {
            self.compare_pressed
                .then(|| self.upscaled_source_for(id))
                .flatten()
        });
        let filmstrip_viewport = filmstrip_viewport_width(self.lightbox_stage_width);
        let filmstrip_range =
            filmstrip_visible_range(selected_index, sequence.len(), filmstrip_viewport);
        let thumbnails = filmstrip_range
            .filter_map(|index| sequence.get(index).copied().map(|key| (index, key)))
            .map(|(index, thumb_key)| {
                let artifact_id = thumb_key.artifact_id();
                let (thumbnail, sharp) = artifact_id
                    .map(|id| self.display_layers(id, StudioPaint::Thumb, window, cx))
                    .unwrap_or((None, None));
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
                let frame = match artifact_id.and_then(|id| self.artifact_menu_conversation(id)) {
                    Some(conversation_id) => self.bind_image_menu(
                        frame,
                        artifact_id.unwrap(),
                        conversation_id,
                        super::image_menu::ImageSurface::Filmstrip,
                        cx,
                    ),
                    None => frame,
                };
                if let Some(thumbnail) = thumbnail
                    && let Some(artifact_id) = artifact_id
                {
                    return frame
                        .child(cover_layers(
                            thumbnail,
                            sharp,
                            px(7.0),
                            Some(SharedString::from(format!(
                                "studio-filmstrip-ready-{}",
                                artifact_id.0
                            ))),
                        ))
                        .into_any_element();
                }
                if let Some(slot) = self.frame_by_key(thumb_key)
                    && let Some((effect, wash)) = super::feed::loading_effect(
                        slot.run_id.0.as_u128() as u32 ^ slot.output_ix as u32,
                        slot.output_ix as u32 % 4,
                        slot.state,
                        slot.progress,
                    )
                {
                    return frame
                        .child(shader(effect).progress(wash).size_full().rounded(px(7.0)))
                        .into_any_element();
                }
                if self
                    .frame_by_key(thumb_key)
                    .is_some_and(ArtifactFrame::is_video)
                {
                    return frame
                        .child(
                            div()
                                .size_full()
                                .flex()
                                .items_center()
                                .justify_center()
                                .text_size(px(11.0))
                                .text_color(theme.text_muted)
                                .child(SharedString::from("▶")),
                        )
                        .into_any_element();
                }
                frame.into_any_element()
            })
            .collect::<Vec<_>>();
        let filmstrip_x = filmstrip_offset(selected_index, filmstrip_viewport);
        let filmstrip_span = filmstrip_content_width(sequence.len());
        let fade_left = filmstrip_x < -0.5;
        let fade_right = filmstrip_x + filmstrip_span > filmstrip_viewport + 0.5;

        let zoomed = self.lightbox_viewer_is_transformed();
        let page_width = self.lightbox_stage_width;
        let mut slides = Vec::new();
        if let Some(compare_source) = compare_source {
            slides.push(self.render_lightbox_slide(
                Some(ArtifactFrameKey::Ready(compare_source)),
                None,
                theme,
                window,
                cx,
            ));
        } else if !lightbox_uses_paging_slides(
            zoomed,
            page_width,
            self.lightbox_swipe_x,
            self.lightbox_snap.is_some(),
        ) {
            slides.push(self.render_lightbox_slide(Some(key), None, theme, window, cx));
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
                Some(key),
                Some((self.lightbox_swipe_x, page_width)),
                theme,
                window,
                cx,
            ));
            if let Some(next_key) = sequence.get(selected_index + 1).copied() {
                slides.push(self.render_lightbox_slide(
                    Some(next_key),
                    Some((self.lightbox_swipe_x + page_width, page_width)),
                    theme,
                    window,
                    cx,
                ));
            }
        }
        let measure_entity = cx.weak_entity();
        let editing = self.edit_target.is_some();
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
                    page.begin_lightbox_pan(event.position, &event.modifiers, cx);
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
            .on_click(cx.listener(|page, event: &gpui::ClickEvent, window, cx| {
                if event.click_count() >= 2 {
                    if page.selected_is_video() {
                        return;
                    }
                    if page.lightbox_zoom > 1.001 || page.lightbox_zoom < 0.999 {
                        page.fit_lightbox(cx);
                    } else {
                        page.adjust_lightbox_zoom(2.0, Some(event.position()), cx);
                    }
                    return;
                }
                if event.click_count() == 1
                    && event.standard_click()
                    && !event.is_keyboard()
                    && page.edit_target.is_none()
                {
                    if page.selected_is_video()
                        && !page.lightbox_click_is_empty(event.position(), window, cx)
                    {
                        return;
                    }
                    if page.lightbox_click_is_empty(event.position(), window, cx) {
                        page.request_close_artifact(cx);
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
                                page.lightbox_stage_origin = bounds.origin;
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
            .children(slides)
            .when(editing, |stage| {
                stage
                    .child(self.render_edit_strokes(window, cx))
                    .child(self.render_brush_size_preview())
            });

        let close_button = div()
            .id("studio-artifact-close")
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
                page.request_close_artifact(cx);
            }))
            .child(
                crate::icons::icon(crate::icons::CLOSE)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );

        let compare_button = id.and_then(|id| self.upscaled_source_for(id)).map(|_| {
            div()
                .id("studio-artifact-compare")
                .size(px(28.0))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.0))
                .occlude()
                .cursor_pointer()
                .when(self.compare_pressed, |button| {
                    button.bg(crate::theme::wash(0.14))
                })
                .hover(|style| style.bg(crate::theme::wash(0.14)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.begin_artifact_compare(cx)),
                )
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.end_artifact_compare(cx)),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.end_artifact_compare(cx)),
                )
                .on_click(|_, _, cx| {
                    cx.stop_propagation();
                })
                .child(
                    crate::icons::icon(crate::icons::COMPARE)
                        .size(px(16.0))
                        .text_color(theme.text_muted.opacity(0.8)),
                )
        });

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

        // Same flush glass column as the chat changes pane: translucent
        // `bg` over the window frost, no extra menu blur or overlay tint.
        let inspector_bg = if theme.is_glass() {
            theme.bg.opacity(0.4)
        } else {
            theme.bg
        };
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
            .bg(inspector_bg)
            .px(px(INSPECTOR_PAD_X))
            .pt(px(if editing {
                Theme::TITLEBAR_TOP_PAD
            } else {
                INSPECTOR_PAD_X
            }))
            .pb(px(16.0));
        let inspector = if editing {
            inspector.child(self.render_precise_edit_sidebar(theme, cx))
        } else {
            inspector
                .when_some(
                    details,
                    |inspector, (turn_id, prompt, model, mime, size, pending, duration, video)| {
                        let has_prompt = !prompt.trim().is_empty();
                        inspector
                            .when(has_prompt, |inspector| {
                                let copy_prompt = prompt.clone();
                                let copied = self.copied_prompt == Some(turn_id);
                                let expanded = self.expanded_inspector_prompts.contains(&turn_id);
                                let clampable = super::feed::prompt_exceeds_lines(
                                    &prompt,
                                    inspector_prompt_inner_width(),
                                    INSPECTOR_PROMPT_ADVANCE,
                                    INSPECTOR_PROMPT_COLLAPSED_LINES,
                                );
                                let collapsed = clampable && !expanded;
                                inspector.child(
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
                                                            * INSPECTOR_PROMPT_COLLAPSED_LINES
                                                                as f32))
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
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.copy_prompt(turn_id, copy_prompt.clone(), cx);
                                            }))
                                            .child(
                                                crate::icons::icon(if copied {
                                                    crate::icons::CHECK
                                                } else {
                                                    crate::icons::COPY
                                                })
                                                .size(px(14.0))
                                                .text_color(theme.text_muted.opacity(0.7)),
                                            ),
                                    ),
                            )
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .when(has_prompt, |meta| meta.mt(px(14.0)))
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(if pending || mime.is_empty() {
                                        model
                                    } else if video {
                                        match duration {
                                            Some(seconds) => format!(
                                                "{model} · {mime} · {} · {:.1} KB",
                                                super::video::format_duration_badge(Some(seconds)),
                                                size as f64 / 1024.0
                                            ),
                                            None => format!(
                                                "{model} · {mime} · {:.1} KB",
                                                size as f64 / 1024.0
                                            ),
                                        }
                                    } else {
                                        format!("{model} · {mime} · {:.1} KB", size as f64 / 1024.0)
                                    })),
                            )
                    },
                )
                .when_some(failed_error, |inspector, message| {
                    inspector.child(
                        div()
                            .flex_none()
                            .mt(px(12.0))
                            .child(render_run_error_chip(theme, &message)),
                    )
                })
                .child(div().flex_1())
                .when_some(id, |inspector, id| {
                    let video = self.artifact_is_video(id);
                    inspector
                        .when(!video, |inspector| {
                            inspector
                                .child(self.render_edit_action(id, theme, cx))
                                .child(div().h(px(8.0)))
                        })
                        .child(self.render_make_video_action(id, theme, cx))
                        .child(div().h(px(8.0)))
                        .when(!video, |inspector| {
                            inspector.child(self.render_artifact_upscale_actions(id, theme, cx))
                        })
                        .child(self.render_inspector_actions(id, theme, cx))
                })
        };

        Some(
            div()
                .size_full()
                .flex()
                .min_w_0()
                .track_focus(&self.focus)
                // The compare trigger is a press-and-hold gesture. Keep the
                // release handler on the whole viewer as well as the button
                // so releasing over the image, inspector, or another chrome
                // element always restores the upscale.
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.end_artifact_compare(cx)),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.end_artifact_compare(cx)),
                )
                .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, window, cx| {
                    if page.dismiss_image_menu(event, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    if page.dismiss_artifact_actions_menu(event, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    if page.dismiss_upscale_settings_menu(event, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    if page.on_edit_model_picker_key(event, window, cx) {
                        cx.stop_propagation();
                        return;
                    }
                    match event.keystroke.key.as_str() {
                        "escape" if page.edit_target.is_some() => {
                            page.exit_edit_mode(cx);
                            cx.stop_propagation();
                        }
                        "escape" => page.request_close_artifact(cx),
                        "space" if page.edit_target.is_none() && page.selected_is_video() => {
                            page.toggle_selected_video(cx);
                            cx.stop_propagation();
                        }
                        "left" if page.edit_target.is_none() => page.navigate_artifact(-1, cx),
                        "right" if page.edit_target.is_none() => page.navigate_artifact(1, cx),
                        "home" if page.edit_target.is_none() => {
                            page.select_artifact_edge(false, cx)
                        }
                        "end" if page.edit_target.is_none() => page.select_artifact_edge(true, cx),
                        "z" if page.edit_target.is_some()
                            && (event.keystroke.modifiers.platform
                                || event.keystroke.modifiers.control)
                            && event.keystroke.modifiers.shift =>
                        {
                            if page.redo_edit_stroke(cx) {
                                cx.stop_propagation();
                            }
                        }
                        "z" if page.edit_target.is_some()
                            && (event.keystroke.modifiers.platform
                                || event.keystroke.modifiers.control) =>
                        {
                            if page.undo_edit_stroke(cx) {
                                cx.stop_propagation();
                            }
                        }
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
                        // Image dissolves through the filmstrip chrome the
                        // same way the transcript fades under the composer.
                        // The strip itself is a sibling so it stays opaque.
                        .child(
                            crate::edge_fade::edge_faded(
                                Theme::TRANSCRIPT_FADE_BAND,
                                false,
                                true,
                                stage,
                            )
                            .band_bottom(if self.edit_target.is_some() {
                                super::edit::EDIT_COMPOSER_HEIGHT
                            } else {
                                ARTIFACT_FILMSTRIP_HEIGHT
                            })
                            // Same band as the filmstrip / edit composer; a
                            // steeper ease-out so the image stays solid and
                            // only dissolves in the last stretch above the
                            // chrome — quadratic lingered half-faded.
                            .ease(crate::edge_fade::EdgeFadeEase::Exponential),
                        )
                        .child(
                            div()
                                .absolute()
                                .top(px(Theme::TITLEBAR_TOP_PAD))
                                .left(px(16.0))
                                .h(px(Theme::TITLEBAR_HEIGHT))
                                .flex()
                                .items_center()
                                .when_some(compare_button, |controls, compare| {
                                    controls.child(compare)
                                }),
                        )
                        .child(
                            div()
                                .absolute()
                                .top(px(Theme::TITLEBAR_TOP_PAD))
                                .right(px(16.0))
                                .h(px(Theme::TITLEBAR_HEIGHT))
                                .flex()
                                .items_center()
                                .child(close_button),
                        )
                        .when(!editing, |column| {
                            column
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
                        })
                        .when(self.edit_target.is_some(), |stage| {
                            stage.child(self.render_edit_composer(theme, cx))
                        })
                        .when(self.edit_target.is_none(), |stage| {
                            stage.child(
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
                            )
                        }),
                )
                .child(
                    div()
                        .relative()
                        .w(px(INSPECTOR_WIDTH))
                        .h_full()
                        .flex_none()
                        .child(inspector)
                        .child(crate::scrollbar::overlay(
                            "studio-inspector",
                            &self.inspector_scroll,
                        )),
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
    fn zoom_around_a_point_keeps_that_screen_point_still() {
        let center = point(500.0, 400.0);
        let focus = point(700.0, 400.0);
        let pan = zoom_pan_around(1.0, point(0.0, 0.0), 2.0, focus, center);
        assert!((pan.x + 200.0).abs() < 0.01);
        assert!(pan.y.abs() < 0.01);
        let back = zoom_pan_around(2.0, pan, 1.0, focus, center);
        assert!(back.x.abs() < 0.01);
        assert!(back.y.abs() < 0.01);
    }

    #[test]
    fn zoom_out_past_fit_rubber_bands() {
        let linear = 0.8;
        let displayed = apply_lightbox_zoom_factor(1.0, linear);
        assert!(
            displayed > linear && displayed < 1.0,
            "displayed={displayed}"
        );
        let further = apply_lightbox_zoom_factor(displayed, linear);
        assert!(
            further < displayed && (displayed - further) < (1.0 - displayed),
            "first={} second={}",
            1.0 - displayed,
            displayed - further
        );
        for logical in [0.95, 0.8, 0.6, 0.4] {
            let shown = display_lightbox_zoom(logical);
            let recovered = logical_lightbox_zoom(shown);
            assert!(
                (recovered - logical).abs() < 0.01,
                "logical={logical} shown={shown} recovered={recovered}"
            );
        }
    }

    #[test]
    fn zoom_spring_settles_on_fit() {
        let mut zoom = 0.82;
        let mut vel = 0.0;
        for _ in 0..240 {
            (zoom, vel) = spring_toward(
                zoom,
                vel,
                1.0,
                1.0 / 60.0,
                ARTIFACT_ZOOM_SPRING_STIFFNESS,
                ARTIFACT_ZOOM_SPRING_DAMPING,
            );
        }
        assert!((zoom - 1.0).abs() < 0.01, "zoom={zoom}");
        assert!(vel.abs() < 0.05, "vel={vel}");
    }

    #[test]
    fn undershoot_zoom_still_paints_the_contained_image() {
        let stage = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1000.0), px(800.0)),
        };
        let painted =
            lightbox_image_paint_bounds(stage, 2000, 1000, 0.8, point(px(-40.0), px(10.0)), 0.0);
        assert!((f32::from(painted.size.width) - 800.0).abs() < 0.5);
        assert!((f32::from(painted.size.height) - 400.0).abs() < 0.5);
        assert!(!lightbox_click_hits_empty(
            stage,
            Some((2000, 1000)),
            0.8,
            point(px(-40.0), px(10.0)),
            0.0,
            point(px(460.0), px(410.0)),
        ));
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
    fn video_stage_fills_the_lightbox_using_aspect() {
        let (width, height) = lightbox_contain_size(1200.0, 800.0, 16.0, 9.0);
        assert!((width - 1200.0).abs() < 0.01);
        assert!((height - 675.0).abs() < 0.01);
        let (width, height) = lightbox_contain_size(800.0, 800.0, 16.0, 9.0);
        assert!((width - 800.0).abs() < 0.01);
        assert!((height - 450.0).abs() < 0.01);
    }

    #[test]
    fn inspector_prompt_inner_width_leaves_room_for_copy() {
        assert!((inspector_prompt_inner_width() - 252.0).abs() < 0.01);
    }

    #[test]
    fn run_error_message_drops_blank_provider_copy() {
        assert_eq!(run_error_message(None), None);
        assert_eq!(run_error_message(Some("")), None);
        assert_eq!(run_error_message(Some("   ")), None);
        assert_eq!(
            run_error_message(Some("  policy violation  ")),
            Some("policy violation")
        );
    }

    #[test]
    fn reapplying_the_route_does_not_steal_a_loading_frame() {
        let ready = StudioArtifactId::new();
        assert!(should_reapply_artifact_selection(None, ready));
        assert!(!should_reapply_artifact_selection(
            Some(ArtifactFrameKey::Ready(ready)),
            ready
        ));
        assert!(should_reapply_artifact_selection(
            Some(ArtifactFrameKey::Ready(StudioArtifactId::new())),
            ready
        ));
        assert!(!should_reapply_artifact_selection(
            Some(ArtifactFrameKey::Loading {
                run_id: StudioRunId::new(),
                output_ix: 0,
            }),
            ready
        ));
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
    fn video_pill_lifts_only_the_filmstrip_deficit() {
        let full = ARTIFACT_FILMSTRIP_HEIGHT
            + ARTIFACT_FILMSTRIP_CLEARANCE
            - crate::video::CONTROLS_INSET;
        // Video fills the stage: the pill clears the strip plus clearance.
        assert!((filmstrip_controls_lift(true, 800.0, 800.0) - full).abs() < 0.01);
        // Partial overlap: only the deficit below the strip's top edge.
        assert!((filmstrip_controls_lift(true, 800.0, 700.0) - (full - 50.0)).abs() < 0.01);
        // Video already ends above the strip: no lift.
        assert_eq!(filmstrip_controls_lift(true, 800.0, 600.0), 0.0);
        // Strip hidden (edit mode): never lift.
        assert_eq!(filmstrip_controls_lift(false, 800.0, 800.0), 0.0);
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
        assert_eq!(decoded.width(), 768);
        assert_eq!(decoded.height(), GALLERY_THUMB_SHORT_EDGE);
    }

    #[test]
    fn gallery_thumbs_bound_extreme_aspect_ratios() {
        assert_eq!(gallery_thumb_dimensions(4096, 1024), (1536, 384));
        assert_eq!(gallery_thumb_dimensions(800, 600), (683, 512));
        assert_eq!(gallery_thumb_dimensions(400, 300), (400, 300));
    }

    #[test]
    fn feed_display_covers_retina_tiles_without_keeping_4k() {
        assert_eq!(feed_display_dimensions(4096, 4096), (1280, 1280));
        // 1080p already fits the short/long caps.
        assert_eq!(feed_display_dimensions(1920, 1080), (1920, 1080));
        assert_eq!(feed_display_dimensions(800, 600), (800, 600));
        assert_eq!(feed_display_dimensions(4096, 1024), (2048, 512));
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
            key: ArtifactFrameKey::Ready(id),
            conversation_id: conversation,
            turn_id: StudioTurnId::new(),
            run_id: StudioRunId::new(),
            output_ix: 0,
            prompt: "prompt".into(),
            model_display_name: "model".into(),
            mime_type: "image/png".into(),
            size_bytes: 1,
            width: Some(1),
            height: Some(1),
            source_artifact_id: None,
            state: StudioRunState::Succeeded,
            progress: None,
            media_kind: MediaKind::Image,
            duration_seconds: None,
            error: None,
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
        assert_eq!(
            frames
                .iter()
                .filter_map(ArtifactFrame::artifact_id)
                .collect::<Vec<_>>(),
            ids
        );
        let neighbors = lightbox_neighbor_ids(&frames, ArtifactFrameKey::Ready(ids[1]));
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
            thumbhash: None,
            content_hash: String::new(),
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
                creating: false,
                done: false,
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
                        video: zeron_studio::VideoModelMeta::default(),
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
                    prompt: None,
                    inputs: Vec::new(),
                    artifacts: vec![artifact.clone()],
                }],
            }],
        };
        let frames = frames_from_conversation(&view);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].conversation_id, conversation_id);
        assert_ne!(frames[0].conversation_id, other);
        assert_eq!(frames[0].artifact_id(), Some(artifact.id));
    }

    #[test]
    fn conversation_frames_include_in_flight_slots() {
        use chrono::Utc;
        use std::collections::BTreeMap;
        let conversation_id = StudioConversationId::new();
        let run_id = StudioRunId::new();
        let view = StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: conversation_id,
                title: "one".into(),
                turn_count: 1,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            turns: vec![zeron_proto::StudioTurnView {
                id: StudioTurnId::new(),
                position: 0,
                prompt: "a fox".into(),
                source_turn_id: None,
                batch_id: zeron_studio::StudioBatchId::new(),
                created_at: Utc::now(),
                runs: vec![zeron_proto::StudioRunView {
                    id: run_id,
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
                        video: zeron_studio::VideoModelMeta::default(),
                        manifest_version: "test".into(),
                        fetched_at: Utc::now(),
                    },
                    controls: BTreeMap::new(),
                    output_count: 1,
                    display_aspect_ratio: (2, 3),
                    state: StudioRunState::Running,
                    progress: None,
                    error: None,
                    quote: None,
                    prompt: Some("a fox".into()),
                    inputs: Vec::new(),
                    artifacts: Vec::new(),
                }],
            }],
        };
        let frames = frames_from_conversation(&view);
        assert_eq!(frames.len(), 1);
        assert_eq!(
            frames[0].key,
            ArtifactFrameKey::Loading {
                run_id,
                output_ix: 0
            }
        );
        assert_eq!(frames[0].prompt, "a fox");
        assert_eq!(frames[0].width, Some(2));
        assert_eq!(frames[0].height, Some(3));
        assert!(frames[0].is_loading());
        assert_eq!(frames[0].error, None);
        assert_eq!(
            resolve_frame_key(
                ArtifactFrameKey::Loading {
                    run_id,
                    output_ix: 0
                },
                &frames
            ),
            Some(frames[0].key)
        );
    }

    #[test]
    fn conversation_frames_keep_a_failed_provider_error() {
        use chrono::Utc;
        use std::collections::BTreeMap;
        let run_id = StudioRunId::new();
        let view = StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: StudioConversationId::new(),
                title: "one".into(),
                turn_count: 1,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            turns: vec![zeron_proto::StudioTurnView {
                id: StudioTurnId::new(),
                position: 0,
                prompt: "a clip".into(),
                source_turn_id: None,
                batch_id: zeron_studio::StudioBatchId::new(),
                created_at: Utc::now(),
                runs: vec![zeron_proto::StudioRunView {
                    id: run_id,
                    position: 0,
                    provider_id: "venice".into(),
                    model: zeron_studio::MediaModel {
                        provider_id: "venice".into(),
                        id: "seedance".into(),
                        display_name: "Seedance".into(),
                        description: None,
                        operation: zeron_studio::MediaOperation::ReferenceToVideo,
                        output_kind: MediaKind::Video,
                        output_mime_types: vec!["video/mp4".into()],
                        input_constraints: Vec::new(),
                        prompt_maximum_chars: None,
                        negative_prompt_maximum_chars: None,
                        maximum_output_count: 1,
                        controls: Vec::new(),
                        pricing: None,
                        features: Vec::new(),
                        video: zeron_studio::VideoModelMeta::default(),
                        manifest_version: "test".into(),
                        fetched_at: Utc::now(),
                    },
                    controls: BTreeMap::new(),
                    output_count: 1,
                    display_aspect_ratio: (9, 16),
                    state: StudioRunState::Failed,
                    progress: None,
                    error: Some(
                        "Your prompt violates the content policy of Venice.ai or the model provider"
                            .into(),
                    ),
                    quote: None,
                    prompt: None,
                    inputs: Vec::new(),
                    artifacts: Vec::new(),
                }],
            }],
        };
        let frames = frames_from_conversation(&view);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].state, StudioRunState::Failed);
        assert_eq!(
            frames[0].error.as_deref(),
            Some("Your prompt violates the content policy of Venice.ai or the model provider")
        );
        assert!(frames[0].is_video());
        assert!(!frames[0].is_loading());
    }

    #[test]
    fn conversation_frames_include_an_upscale_from_its_own_turn() {
        use chrono::Utc;
        use std::collections::BTreeMap;
        use zeron_studio::{GenerationInput, GenerationInputSource};
        let source_id = StudioArtifactId::new();
        let upscale_id = StudioArtifactId::new();
        let generate_turn = StudioTurnId::new();
        let upscale_turn = StudioTurnId::new();
        let artifact = |id: StudioArtifactId| zeron_proto::StudioArtifactView {
            id,
            output_position: 0,
            media_kind: MediaKind::Image,
            mime_type: "image/png".into(),
            size_bytes: 1,
            width: Some(1),
            height: Some(1),
            duration_seconds: None,
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
            thumbhash: None,
            content_hash: String::new(),
        };
        let model =
            |operation: zeron_studio::MediaOperation, name: &str| zeron_studio::MediaModel {
                provider_id: "venice".into(),
                id: name.into(),
                display_name: name.into(),
                description: None,
                operation,
                output_kind: MediaKind::Image,
                output_mime_types: vec!["image/png".into()],
                input_constraints: Vec::new(),
                prompt_maximum_chars: None,
                negative_prompt_maximum_chars: None,
                maximum_output_count: 4,
                controls: Vec::new(),
                pricing: None,
                features: Vec::new(),
                video: zeron_studio::VideoModelMeta::default(),
                manifest_version: "test".into(),
                fetched_at: Utc::now(),
            };
        let view = StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: StudioConversationId::new(),
                title: "one".into(),
                turn_count: 2,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            turns: vec![
                zeron_proto::StudioTurnView {
                    id: generate_turn,
                    position: 0,
                    prompt: "a fox".into(),
                    source_turn_id: None,
                    batch_id: zeron_studio::StudioBatchId::new(),
                    created_at: Utc::now(),
                    runs: vec![zeron_proto::StudioRunView {
                        id: StudioRunId::new(),
                        position: 0,
                        provider_id: "venice".into(),
                        model: model(zeron_studio::MediaOperation::TextToImage, "flux"),
                        controls: BTreeMap::new(),
                        output_count: 1,
                        display_aspect_ratio: (1, 1),
                        state: StudioRunState::Succeeded,
                        progress: None,
                        error: None,
                        quote: None,
                        prompt: None,
                        inputs: Vec::new(),
                        artifacts: vec![artifact(source_id)],
                    }],
                },
                zeron_proto::StudioTurnView {
                    id: upscale_turn,
                    position: 1,
                    prompt: String::new(),
                    source_turn_id: Some(generate_turn),
                    batch_id: zeron_studio::StudioBatchId::new(),
                    created_at: Utc::now(),
                    runs: vec![zeron_proto::StudioRunView {
                        id: StudioRunId::new(),
                        position: 0,
                        provider_id: "venice".into(),
                        model: model(zeron_studio::MediaOperation::Upscale, "upscale"),
                        controls: BTreeMap::new(),
                        output_count: 1,
                        display_aspect_ratio: (1, 1),
                        state: StudioRunState::Succeeded,
                        progress: None,
                        error: None,
                        quote: None,
                        prompt: None,
                        inputs: vec![GenerationInput {
                            role: "source".into(),
                            ordinal: 0,
                            source: GenerationInputSource::Artifact {
                                artifact_id: source_id,
                            },
                            content_hash: String::new(),
                        }],
                        artifacts: vec![artifact(upscale_id)],
                    }],
                },
            ],
        };
        let frames = frames_from_conversation(&view);
        let ids: Vec<_> = frames
            .iter()
            .filter_map(ArtifactFrame::artifact_id)
            .collect();
        assert_eq!(ids, vec![source_id, upscale_id]);
        assert_eq!(
            resolve_frame_key(ArtifactFrameKey::Ready(upscale_id), &frames),
            Some(ArtifactFrameKey::Ready(upscale_id))
        );
    }

    #[test]
    fn conversation_frames_include_video_and_keep_swipe_order() {
        use chrono::Utc;
        use std::collections::BTreeMap;
        let conversation_id = StudioConversationId::new();
        let image_id = StudioArtifactId::new();
        let video_id = StudioArtifactId::new();
        let artifact = |id: StudioArtifactId, kind: MediaKind, mime: &str, duration| {
            zeron_proto::StudioArtifactView {
                id,
                output_position: 0,
                media_kind: kind,
                mime_type: mime.into(),
                size_bytes: 1,
                width: Some(16),
                height: Some(9),
                duration_seconds: duration,
                metadata: serde_json::Value::Null,
                created_at: Utc::now(),
                thumbhash: None,
                content_hash: String::new(),
            }
        };
        let model = |operation: zeron_studio::MediaOperation, kind: MediaKind, name: &str| {
            zeron_studio::MediaModel {
                provider_id: "venice".into(),
                id: name.into(),
                display_name: name.into(),
                description: None,
                operation,
                output_kind: kind,
                output_mime_types: vec![if kind == MediaKind::Video {
                    "video/mp4".into()
                } else {
                    "image/png".into()
                }],
                input_constraints: Vec::new(),
                prompt_maximum_chars: None,
                negative_prompt_maximum_chars: None,
                maximum_output_count: 4,
                controls: Vec::new(),
                pricing: None,
                features: Vec::new(),
                video: zeron_studio::VideoModelMeta::default(),
                manifest_version: "test".into(),
                fetched_at: Utc::now(),
            }
        };
        let view = StudioConversationView {
            conversation: zeron_proto::StudioConversationSummary {
                id: conversation_id,
                title: "mixed".into(),
                turn_count: 2,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            turns: vec![
                zeron_proto::StudioTurnView {
                    id: StudioTurnId::new(),
                    position: 0,
                    prompt: "a fox".into(),
                    source_turn_id: None,
                    batch_id: zeron_studio::StudioBatchId::new(),
                    created_at: Utc::now(),
                    runs: vec![zeron_proto::StudioRunView {
                        id: StudioRunId::new(),
                        position: 0,
                        provider_id: "venice".into(),
                        model: model(
                            zeron_studio::MediaOperation::TextToImage,
                            MediaKind::Image,
                            "flux",
                        ),
                        controls: BTreeMap::new(),
                        output_count: 1,
                        display_aspect_ratio: (1, 1),
                        state: StudioRunState::Succeeded,
                        progress: None,
                        error: None,
                        quote: None,
                        prompt: None,
                        inputs: Vec::new(),
                        artifacts: vec![artifact(image_id, MediaKind::Image, "image/png", None)],
                    }],
                },
                zeron_proto::StudioTurnView {
                    id: StudioTurnId::new(),
                    position: 1,
                    prompt: "the fox runs".into(),
                    source_turn_id: None,
                    batch_id: zeron_studio::StudioBatchId::new(),
                    created_at: Utc::now(),
                    runs: vec![zeron_proto::StudioRunView {
                        id: StudioRunId::new(),
                        position: 0,
                        provider_id: "venice".into(),
                        model: model(
                            zeron_studio::MediaOperation::TextToVideo,
                            MediaKind::Video,
                            "seedance",
                        ),
                        controls: BTreeMap::new(),
                        output_count: 1,
                        display_aspect_ratio: (16, 9),
                        state: StudioRunState::Succeeded,
                        progress: None,
                        error: None,
                        quote: None,
                        prompt: None,
                        inputs: Vec::new(),
                        artifacts: vec![artifact(
                            video_id,
                            MediaKind::Video,
                            "video/mp4",
                            Some(6.0),
                        )],
                    }],
                },
            ],
        };
        let frames = frames_from_conversation(&view);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].artifact_id(), Some(image_id));
        assert!(!frames[0].is_video());
        assert_eq!(frames[1].artifact_id(), Some(video_id));
        assert!(frames[1].is_video());
        assert_eq!(frames[1].duration_seconds, Some(6.0));
        let neighbors = lightbox_neighbor_ids(&frames, ArtifactFrameKey::Ready(image_id));
        assert!(neighbors.contains(&video_id));
        assert_eq!(
            resolve_frame_key(ArtifactFrameKey::Ready(video_id), &frames),
            Some(ArtifactFrameKey::Ready(video_id))
        );
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
    fn clipboard_image_uses_mime_then_sniffs_bytes() {
        let png = clipboard_image_from_bytes("image/png", b"\x89PNG\r\n\x1a\nrest".to_vec())
            .expect("png mime");
        assert_eq!(png.format, ImageFormat::Png);

        let jpeg =
            clipboard_image_from_bytes("", vec![0xff, 0xd8, 0xff, 0xe0]).expect("jpeg magic");
        assert_eq!(jpeg.format, ImageFormat::Jpeg);

        assert!(clipboard_image_from_bytes("text/plain", b"not an image".to_vec()).is_err());
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

    #[test]
    fn contained_2_3_box_is_narrower_than_the_thumbhash_smear() {
        let (width, height) = lightbox_contain_size(1200.0, 800.0, 2.0, 3.0);
        assert!((height - 800.0).abs() < 0.01);
        assert!((width - 800.0 * 2.0 / 3.0).abs() < 0.01);
        let (thumb_w, _) = lightbox_contain_size(1200.0, 800.0, 5.0, 7.0);
        assert!(
            thumb_w - width > 20.0,
            "5:7 thumbhash {thumb_w} should outrun a 2:3 photo {width}"
        );
        let (nine_sixteen, _) = lightbox_contain_size(1200.0, 800.0, 9.0, 16.0);
        let (decoded_nine_sixteen, _) = lightbox_contain_size(1200.0, 800.0, 18.0, 32.0);
        assert!((nine_sixteen - decoded_nine_sixteen).abs() < 0.01);
    }

    #[test]
    fn empty_space_is_the_letterbox_around_the_contained_image() {
        let stage = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(1000.0), px(800.0)),
        };
        let pan = Point::default();
        let painted = lightbox_image_paint_bounds(stage, 2000, 1000, 1.0, pan, 0.0);
        assert!((f32::from(painted.size.width) - 1000.0).abs() < 0.5);
        assert!((f32::from(painted.size.height) - 500.0).abs() < 0.5);
        assert!(lightbox_click_hits_empty(
            stage,
            Some((2000, 1000)),
            1.0,
            pan,
            0.0,
            point(px(500.0), px(50.0)),
        ));
        assert!(!lightbox_click_hits_empty(
            stage,
            Some((2000, 1000)),
            1.0,
            pan,
            0.0,
            point(px(500.0), px(400.0)),
        ));
        assert!(lightbox_click_hits_empty(
            stage,
            None,
            1.0,
            pan,
            0.0,
            point(px(500.0), px(400.0)),
        ));
    }

    #[test]
    fn empty_space_follows_zoom_and_swipe() {
        let stage = Bounds {
            origin: point(px(100.0), px(40.0)),
            size: size(px(1000.0), px(800.0)),
        };
        let pan = Point::default();
        // Zoomed 2×: the contained 1000×500 box becomes 2000×1000, centered
        // on the stage, so the original letterbox is now inside the image.
        assert!(!lightbox_click_hits_empty(
            stage,
            Some((2000, 1000)),
            2.0,
            pan,
            0.0,
            point(px(600.0), px(90.0)),
        ));
        // Mid-swipe the current frame sits `swipe_x` to the side.
        assert!(lightbox_click_hits_empty(
            stage,
            Some((2000, 1000)),
            1.0,
            pan,
            -400.0,
            point(px(900.0), px(440.0)),
        ));
        assert!(!lightbox_click_hits_empty(
            stage,
            Some((2000, 1000)),
            1.0,
            pan,
            -400.0,
            point(px(200.0), px(440.0)),
        ));
    }
}
