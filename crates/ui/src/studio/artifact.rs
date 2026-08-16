//! Routed artifact viewer: full-bleed image strip, filmstrip, and inspector.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use gpui::{
    AnyElement, ClipboardItem, Context, Image, ImageFormat, ObjectFit, PinchEvent, Pixels, Point,
    ScrollWheelEvent, SharedString, TouchPhase, canvas, div, img, prelude::*, px,
};
use zeron_rpc::methods;
use zeron_studio::{StudioArtifactId, StudioConversationId};

use crate::state::EngineHandle;
use crate::theme::Theme;

use super::StudioEvent;
use super::page::StudioPage;

/// Fraction of the stage width that commits a page turn on release.
const ARTIFACT_SWIPE_COMMIT_FRACTION: f32 = 0.18;
const ARTIFACT_SWIPE_COMMIT_MIN: f32 = 56.0;
/// Horizontal flick speed (px/s) that commits even before the distance gate.
const ARTIFACT_SWIPE_FLICK: f32 = 650.0;
const ARTIFACT_SWIPE_EDGE_RESISTANCE: f32 = 0.28;
const ARTIFACT_SWIPE_EDGE_LIMIT_FRACTION: f32 = 0.22;
/// Slightly overdamped snap. Underdamped leftovers after a flick flew
/// through the next page; ζ > 1 kills that bounce. ω ≈ 36 settles ~150ms.
const ARTIFACT_SNAP_OMEGA: f32 = 36.0;
const ARTIFACT_SNAP_ZETA: f32 = 1.08;
const ARTIFACT_FILMSTRIP_STEP: f32 = 38.0;
/// macOS sends inertial wheel events after TouchPhase::Ended. Drop them
/// until the next finger-down so a flick cannot start a second page turn.
const ARTIFACT_SWIPE_INERTIA_GUARD: Duration = Duration::from_millis(180);

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

/// Spring `position` toward 0. `velocity` is px/s.
fn step_lightbox_snap_spring(mut position: f32, mut velocity: f32, mut dt: f32) -> (f32, f32) {
    dt = dt.clamp(1.0 / 240.0, 1.0 / 20.0);
    while dt > 0.0 {
        let step = dt.min(1.0 / 60.0);
        dt -= step;
        let accel = -ARTIFACT_SNAP_OMEGA * ARTIFACT_SNAP_OMEGA * position
            - 2.0 * ARTIFACT_SNAP_ZETA * ARTIFACT_SNAP_OMEGA * velocity;
        velocity += accel * step;
        position += velocity * step;
    }
    (position, velocity)
}

/// `+1` selects the next image (swipe left), `-1` the previous, `0` stays.
fn lightbox_swipe_commit_delta(
    offset: f32,
    velocity: f32,
    width: f32,
    can_prev: bool,
    can_next: bool,
) -> isize {
    let width = width.max(1.0);
    let distance = (width * ARTIFACT_SWIPE_COMMIT_FRACTION).max(ARTIFACT_SWIPE_COMMIT_MIN);
    if can_next && (offset <= -distance || (offset < 0.0 && velocity <= -ARTIFACT_SWIPE_FLICK)) {
        1
    } else if can_prev && (offset >= distance || (offset > 0.0 && velocity >= ARTIFACT_SWIPE_FLICK))
    {
        -1
    } else {
        0
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
        } else if delta > 0.0 {
            (offset + delta * ARTIFACT_SWIPE_EDGE_RESISTANCE)
                .clamp(0.0, width * ARTIFACT_SWIPE_EDGE_LIMIT_FRACTION)
        } else {
            (offset + delta).max(0.0)
        }
    } else if proposed < 0.0 {
        if can_next {
            proposed.max(-width)
        } else if delta < 0.0 {
            (offset + delta * ARTIFACT_SWIPE_EDGE_RESISTANCE)
                .clamp(-width * ARTIFACT_SWIPE_EDGE_LIMIT_FRACTION, 0.0)
        } else {
            (offset + delta).min(0.0)
        }
    } else {
        0.0
    }
}

fn remap_lightbox_swipe_after_commit(offset: f32, delta: isize, width: f32) -> f32 {
    offset + delta as f32 * width.max(1.0)
}

/// Cap leftover flick speed so the snap cannot fly through 0.
fn clip_snap_velocity(position: f32, velocity: f32) -> f32 {
    if position.abs() < f32::EPSILON {
        return 0.0;
    }
    let limit = (ARTIFACT_SNAP_OMEGA * position.abs() * 0.35).min(1600.0);
    if velocity * position < 0.0 {
        (-position.signum() * velocity.abs()).clamp(-limit, limit)
    } else {
        velocity.clamp(-limit * 0.2, limit * 0.2)
    }
}

pub(super) fn write_artifact_file(destination: PathBuf, bytes: Vec<u8>) -> Result<(), String> {
    std::fs::write(destination, bytes).map_err(|error| error.to_string())
}

pub(super) async fn read_artifact_image(
    engine: &EngineHandle,
    artifact_id: StudioArtifactId,
) -> Result<Arc<Image>, String> {
    let (_, mime, bytes) = read_artifact_bytes(engine, artifact_id).await?;
    let format = ImageFormat::from_mime_type(&mime).unwrap_or(ImageFormat::Png);
    Ok(Arc::new(Image::from_bytes(format, bytes)))
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
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DELETE_STUDIO_ARTIFACT,
                    serde_json::json!({ "artifactId": artifact_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.selected_artifact = None;
                        page.images.remove(&artifact_id);
                        cx.emit(StudioEvent::CloseArtifact);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn download_artifact(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let suggested = self
            .conversation
            .iter()
            .flat_map(|view| &view.turns)
            .flat_map(|turn| &turn.runs)
            .flat_map(|run| &run.artifacts)
            .find(|artifact| artifact.id == artifact_id)
            .map(|artifact| {
                let extension = match artifact.mime_type.as_str() {
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    _ => "png",
                };
                format!("studio-{}.{}", artifact_id.0, extension)
            })
            .unwrap_or_else(|| format!("studio-{}.png", artifact_id.0));
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
        self.conversation
            .iter()
            .flat_map(|view| &view.turns)
            .flat_map(|turn| &turn.runs)
            .flat_map(|run| &run.artifacts)
            .map(|artifact| artifact.id)
            .collect()
    }

    pub(super) fn reset_lightbox_swipe(&mut self) {
        self.lightbox_swipe_x = 0.0;
        self.lightbox_swipe_velocity = 0.0;
        self.lightbox_swipe_spring = false;
        self.lightbox_swipe_last_tick = None;
    }

    pub(super) fn reset_lightbox_viewer(&mut self) {
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        self.lightbox_drag = None;
        self.reset_lightbox_swipe();
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
        self.artifact_filmstrip_scroll.scroll_to_item(index);
        if let Some(conversation_id) = self.selected_conversation {
            cx.emit(StudioEvent::OpenArtifact {
                conversation_id,
                artifact_id,
            });
        }
        cx.notify();
        changed
    }

    pub(super) fn select_artifact_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let changed = self.adopt_artifact_index(index, cx);
        self.reset_lightbox_swipe();
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
        let mut changed = false;
        if self.selected_conversation != Some(conversation_id) {
            self.open_conversation(conversation_id, cx);
            changed = true;
        }
        if self.selected_artifact != Some(artifact_id) {
            self.selected_artifact = Some(artifact_id);
            self.reset_lightbox_viewer();
            if let Some(index) = self
                .artifact_sequence()
                .iter()
                .position(|candidate| *candidate == artifact_id)
            {
                self.artifact_filmstrip_scroll.scroll_to_item(index);
            }
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    pub fn close_artifact(&mut self, cx: &mut Context<Self>) {
        if self.selected_artifact.take().is_some() {
            self.reset_lightbox_viewer();
            cx.notify();
        }
    }

    pub(super) fn lightbox_page_width(&self) -> f32 {
        self.lightbox_stage_width.max(1.0)
    }

    pub(super) fn adjust_lightbox_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.lightbox_zoom = (self.lightbox_zoom * factor).clamp(1.0, 8.0);
        if self.lightbox_zoom > 1.001 {
            self.reset_lightbox_swipe();
        }
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

    pub(super) fn wake_lightbox_swipe_spring(&mut self, velocity: f32, cx: &mut Context<Self>) {
        self.lightbox_swipe_velocity = velocity;
        self.lightbox_swipe_spring = true;
        self.lightbox_swipe_last_tick = None;
        cx.notify();
    }

    pub(super) fn step_lightbox_swipe_spring(&mut self, cx: &mut Context<Self>) {
        if !self.lightbox_swipe_spring {
            return;
        }
        let now = Instant::now();
        let dt = self
            .lightbox_swipe_last_tick
            .map(|last| now.duration_since(last).as_secs_f32())
            .unwrap_or(1.0 / 60.0);
        self.lightbox_swipe_last_tick = Some(now);
        (self.lightbox_swipe_x, self.lightbox_swipe_velocity) =
            step_lightbox_snap_spring(self.lightbox_swipe_x, self.lightbox_swipe_velocity, dt);
        if self.lightbox_swipe_x.abs() < 0.35 && self.lightbox_swipe_velocity.abs() < 20.0 {
            self.reset_lightbox_swipe();
        }
        cx.notify();
    }

    pub(super) fn finish_lightbox_swipe(&mut self, cx: &mut Context<Self>) {
        let width = self.lightbox_page_width();
        let offset = self.lightbox_swipe_x;
        let release_velocity = self.lightbox_swipe_velocity;
        let artifacts = self.artifact_sequence();
        let index = self
            .selected_artifact
            .and_then(|selected| artifacts.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let delta = lightbox_swipe_commit_delta(
            offset,
            release_velocity,
            width,
            index > 0,
            index + 1 < artifacts.len(),
        );
        if delta != 0 {
            let next = stepped_artifact_index(index, artifacts.len(), delta, false);
            if next != index && self.adopt_artifact_index(next, cx) {
                self.lightbox_swipe_x = remap_lightbox_swipe_after_commit(offset, delta, width);
            }
        }
        self.lightbox_ignore_scroll_until = Some(Instant::now() + ARTIFACT_SWIPE_INERTIA_GUARD);
        if crate::motion::reduced_motion(cx) {
            self.reset_lightbox_swipe();
            cx.notify();
            return;
        }
        let velocity = clip_snap_velocity(self.lightbox_swipe_x, release_velocity);
        self.wake_lightbox_swipe_spring(velocity, cx);
    }

    pub(super) fn on_lightbox_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(16.0));
        let horizontal = f32::from(delta.x);
        let vertical = f32::from(delta.y);
        if event.touch_phase == TouchPhase::Started {
            self.lightbox_ignore_scroll_until = None;
            self.lightbox_swipe_spring = false;
            self.lightbox_swipe_last_tick = None;
        }
        if self
            .lightbox_ignore_scroll_until
            .is_some_and(|until| Instant::now() < until)
        {
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
        if vertical.abs() > horizontal.abs() {
            let factor = (vertical * 0.004).exp();
            self.adjust_lightbox_zoom(factor, cx);
            if self.lightbox_zoom <= 1.001 {
                self.fit_lightbox(cx);
            }
            cx.stop_propagation();
            return;
        }
        if horizontal.abs() < f32::EPSILON {
            cx.stop_propagation();
            return;
        }
        if !event.delta.precise() {
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
        let limit = 420.0 * (self.lightbox_zoom - 1.0).max(0.0);
        self.lightbox_pan.x = px((f32::from(self.lightbox_pan.x) + dx).clamp(-limit, limit));
        self.lightbox_pan.y = px((f32::from(self.lightbox_pan.y) + dy).clamp(-limit, limit));
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
    }

    pub(super) fn render_lightbox_slide(
        &self,
        artifact_id: Option<StudioArtifactId>,
        page: Option<(f32, f32)>,
        theme: &Theme,
    ) -> AnyElement {
        let image = artifact_id.and_then(|id| self.images.get(&id).cloned());
        let frame = if let Some((left, width)) = page {
            div()
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
            div()
                .absolute()
                .inset_0()
                .left(self.lightbox_pan.x)
                .top(self.lightbox_pan.y)
                .flex()
                .items_center()
                .justify_center()
        };
        let zoom = if page.is_some() {
            1.0
        } else {
            self.lightbox_zoom
        };
        if let Some(image) = image {
            frame
                .child(
                    img(image)
                        .w(gpui::relative(zoom))
                        .h(gpui::relative(zoom))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element()
        } else {
            frame
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_faint)
                        .child("Loading image…"),
                )
                .into_any_element()
        }
    }

    pub(super) fn render_artifact_page(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = self.selected_artifact?;
        let sequence = self.artifact_sequence();
        let selected_index = sequence
            .iter()
            .position(|candidate| *candidate == id)
            .unwrap_or(0);
        let details = self.conversation.as_ref().and_then(|view| {
            view.turns.iter().find_map(|turn| {
                turn.runs.iter().find_map(|run| {
                    run.artifacts
                        .iter()
                        .find(|artifact| artifact.id == id)
                        .map(|artifact| {
                            (
                                turn.prompt.clone(),
                                run.model.display_name.clone(),
                                artifact.mime_type.clone(),
                                artifact.size_bytes,
                            )
                        })
                })
            })
        });
        let thumbnails = sequence
            .iter()
            .enumerate()
            .map(|(index, artifact_id)| {
                let thumbnail = self.images.get(artifact_id).cloned();
                let frame_size = if index == selected_index { 58.0 } else { 50.0 };
                div()
                    .id(SharedString::from(format!("studio-thumbnail-{index}")))
                    .size(px(frame_size))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(if index == selected_index {
                        theme.text_muted
                    } else {
                        theme.border
                    })
                    .bg(crate::theme::wash(0.04))
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.82))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.select_artifact_index(index, cx);
                    }))
                    .when_some(thumbnail, |thumb, thumbnail| {
                        thumb.child(
                            div()
                                .size(px(frame_size - 2.0))
                                .flex_none()
                                .rounded(px(7.0))
                                .overflow_hidden()
                                .child(
                                    img(thumbnail)
                                        .size_full()
                                        .rounded(px(7.0))
                                        .object_fit(ObjectFit::Cover),
                                ),
                        )
                    })
            })
            .collect::<Vec<_>>();

        let zoomed = self.lightbox_zoom > 1.001;
        let page_width = self.lightbox_stage_width;
        let mut slides = Vec::new();
        if zoomed || page_width <= 1.0 {
            slides.push(self.render_lightbox_slide(Some(id), None, theme));
        } else {
            if selected_index > 0 {
                slides.push(self.render_lightbox_slide(
                    Some(sequence[selected_index - 1]),
                    Some((self.lightbox_swipe_x - page_width, page_width)),
                    theme,
                ));
            }
            slides.push(self.render_lightbox_slide(
                Some(id),
                Some((self.lightbox_swipe_x, page_width)),
                theme,
            ));
            if let Some(next_id) = sequence.get(selected_index + 1).copied() {
                slides.push(self.render_lightbox_slide(
                    Some(next_id),
                    Some((self.lightbox_swipe_x + page_width, page_width)),
                    theme,
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
                    move |bounds, _, cx| {
                        let width = f32::from(bounds.size.width);
                        measure_entity
                            .update(cx, |page, cx| {
                                if (page.lightbox_stage_width - width).abs() > 0.5 {
                                    page.lightbox_stage_width = width;
                                    cx.notify();
                                }
                            })
                            .ok();
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
            .size_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.glass_overlay())
            .px(px(18.0))
            .pt(px(Theme::TITLEBAR_HEIGHT + 18.0))
            .pb(px(16.0))
            .when_some(details, |inspector, (prompt, model, mime, size)| {
                let copy_prompt = prompt.clone();
                inspector
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(prompt)),
                            )
                            .child(
                                div()
                                    .id("studio-copy-prompt")
                                    .size(px(24.0))
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
                            .mt(px(14.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "{model} · {mime} · {:.1} KB",
                                size as f64 / 1024.0
                            ))),
                    )
            })
            .child(div().flex_1())
            .child(
                div()
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
                                .h(px(78.0))
                                .overflow_x_scroll()
                                .track_scroll(&self.artifact_filmstrip_scroll)
                                .on_scroll_wheel(cx.listener(|page, event, _, cx| {
                                    page.on_filmstrip_scroll(event, cx)
                                }))
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(8.0))
                                .px(px(16.0))
                                .occlude()
                                .children(thumbnails),
                        ),
                )
                .child(
                    div()
                        .w(px(320.0))
                        .h_full()
                        .flex_none()
                        .child(crate::frost::frosted(
                            0.0,
                            crate::frost::MENU_BLUR,
                            inspector,
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
    fn lightbox_snap_spring_settles_from_a_page_width() {
        let (mut position, mut velocity) = (800.0, 0.0);
        for _ in 0..90 {
            (position, velocity) = step_lightbox_snap_spring(position, velocity, 1.0 / 60.0);
        }
        assert!(position.abs() < 0.35, "position={position}");
        assert!(velocity.abs() < 20.0, "velocity={velocity}");
    }

    #[test]
    fn lightbox_swipe_commits_on_distance_or_flick() {
        assert_eq!(
            lightbox_swipe_commit_delta(-200.0, 0.0, 800.0, true, true),
            1
        );
        assert_eq!(
            lightbox_swipe_commit_delta(200.0, 0.0, 800.0, true, true),
            -1
        );
        assert_eq!(
            lightbox_swipe_commit_delta(-40.0, 0.0, 800.0, true, true),
            0
        );
        assert_eq!(
            lightbox_swipe_commit_delta(-40.0, -800.0, 800.0, true, true),
            1
        );
        assert_eq!(
            lightbox_swipe_commit_delta(-200.0, 0.0, 800.0, true, false),
            0
        );
        assert_eq!(
            lightbox_swipe_commit_delta(200.0, 0.0, 800.0, false, true),
            0
        );
    }

    #[test]
    fn lightbox_swipe_clamps_to_one_page_and_rubber_bands_edges() {
        assert_eq!(
            apply_lightbox_swipe_delta(0.0, -200.0, 800.0, true, true),
            -200.0
        );
        assert_eq!(
            apply_lightbox_swipe_delta(0.0, -900.0, 800.0, true, true),
            -800.0
        );
        let resisted = apply_lightbox_swipe_delta(0.0, -200.0, 800.0, true, false);
        assert!(resisted < 0.0 && resisted > -200.0);
        let returning = apply_lightbox_swipe_delta(-20.0, 20.0, 800.0, true, false);
        assert_eq!(returning, 0.0);
    }

    #[test]
    fn lightbox_swipe_remap_keeps_the_visual_page() {
        assert_eq!(remap_lightbox_swipe_after_commit(-200.0, 1, 800.0), 600.0);
        assert_eq!(remap_lightbox_swipe_after_commit(200.0, -1, 800.0), -600.0);
    }

    #[test]
    fn lightbox_snap_does_not_fly_through_the_page() {
        let start = remap_lightbox_swipe_after_commit(-200.0, 1, 800.0);
        let mut velocity = clip_snap_velocity(start, -4000.0);
        let mut position = start;
        let mut farthest = start;
        for _ in 0..90 {
            (position, velocity) = step_lightbox_snap_spring(position, velocity, 1.0 / 60.0);
            farthest = farthest.min(position);
        }
        assert!(position.abs() < 0.5, "position={position}");
        assert!(
            farthest > -80.0,
            "overshot past the page: farthest={farthest}"
        );
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
