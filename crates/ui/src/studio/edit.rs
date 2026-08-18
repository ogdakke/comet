//! Lightbox image-edit mode: slim composer, brush, and derived-run submit.

use gpui::{
    AnyElement, Bounds, Context, Focusable as _, MouseButton, PathBuilder, Pixels, Point,
    SharedString, Window, canvas, div, point, prelude::*, px, size,
};
use zeron_proto::StudioConversationView;
use zeron_rpc::methods;
use zeron_studio::{MediaKind, MediaModel, MediaOperation, ModelId, StudioArtifactId};

use crate::composer::{COMPACT_TOTAL_HEIGHT, INPUT_LINE_HEIGHT};
use crate::theme::Theme;

use super::page::StudioPage;
use super::paint::PaintSession;

/// Fade band under the floating pill: compact chat height plus bottom inset.
const EDIT_COMPOSER_BOTTOM: f32 = 10.0;
pub(super) const EDIT_COMPOSER_HEIGHT: f32 = COMPACT_TOTAL_HEIGHT + EDIT_COMPOSER_BOTTOM;
const BRUSH_TRACK_HEIGHT: f32 = 132.0;
const DEFAULT_BRUSH_T: f32 = 0.28;

impl StudioPage {
    pub(super) fn editing_artifact(&self) -> Option<StudioArtifactId> {
        self.edit_target
    }

    pub(super) fn edit_models(&self) -> impl Iterator<Item = &MediaModel> {
        self.edit_models.iter().filter(|model| {
            model.operation == MediaOperation::ImageEdit && model.output_kind == MediaKind::Image
        })
    }

    pub(super) fn edit_is_available(&self) -> bool {
        self.edit_models().next().is_some()
    }

    pub(super) fn toggle_edit_mode(
        &mut self,
        artifact_id: StudioArtifactId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.edit_target == Some(artifact_id) {
            self.exit_edit_mode(cx);
            return;
        }
        self.enter_edit_mode(artifact_id, window, cx);
    }

    pub(super) fn enter_edit_mode(
        &mut self,
        artifact_id: StudioArtifactId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.edit_is_available() {
            self.error = Some("No edit model is available".into());
            cx.notify();
            return;
        }
        let (width, height) = self
            .artifact_frame(artifact_id)
            .and_then(|frame| frame.width.zip(frame.height))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or((1024, 1024));
        self.edit_target = Some(artifact_id);
        self.edit_paint = Some(PaintSession::new(width, height));
        self.edit_brush_t = DEFAULT_BRUSH_T;
        self.edit_space_pan = false;
        self.edit_prompt.update(cx, |input, cx| {
            input.set_text(String::new(), cx);
        });
        window.focus(&self.edit_prompt.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn exit_edit_mode(&mut self, cx: &mut Context<Self>) {
        self.edit_target = None;
        self.edit_paint = None;
        self.edit_space_pan = false;
        self.edit_brush_drag = false;
        cx.notify();
    }

    pub(super) fn submit_edit(&mut self, cx: &mut Context<Self>) {
        let Some(source_id) = self.edit_target else {
            return;
        };
        let Some(engine) = self.engine(cx) else {
            self.error = Some("Engine not connected".into());
            return;
        };
        let Some(conversation_id) = self.artifact_menu_conversation(source_id) else {
            self.error = Some("This image is not attached to a conversation".into());
            return;
        };
        let prompt = self.edit_prompt.read(cx).text().trim().to_owned();
        if prompt.is_empty() {
            return;
        }
        let Some(model) = self.resolve_edit_model(source_id) else {
            self.error = Some("No edit model is available".into());
            return;
        };
        let display_aspect_ratio = self.display_aspect_for(source_id);
        let mask_png_base64 = self
            .edit_paint
            .as_ref()
            .and_then(PaintSession::mask_png)
            .map(|bytes| base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes));

        self.remembered.last_edit_model_id = Some(model.id.clone());
        self.persist_composer_defaults(cx);
        self.pending_edit_source = Some(source_id);
        self.exit_edit_mode(cx);

        let provider_id = model.provider_id.clone();
        let model_id = model.id.clone();
        let operation = model.operation;
        let manifest_version = model.manifest_version.clone();
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::APPEND_STUDIO_DERIVED_RUN,
                    serde_json::json!({
                        "sourceArtifactId": source_id,
                        "prompt": prompt,
                        "maskPngBase64": mask_png_base64,
                        "run": {
                            "providerId": provider_id,
                            "modelId": model_id,
                            "operation": operation,
                            "outputCount": 1,
                            "controls": {},
                            "inputs": [],
                            "manifestVersion": manifest_version,
                            "displayAspectRatio": display_aspect_ratio,
                        },
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                if let Err(error) = result {
                    page.pending_edit_source = None;
                    page.error = Some(error.to_string().into());
                } else if page.selected_conversation == Some(conversation_id)
                    && let Some(view) = page.conversation.clone()
                {
                    page.select_pending_edit(&view, source_id, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn select_pending_edit(
        &mut self,
        view: &StudioConversationView,
        source_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let child = super::lineage::lineage_tiles(view)
            .into_iter()
            .rev()
            .find(|tile| tile.source_artifact_id == Some(source_id));
        if let Some(tile) = child {
            if let Some(artifact_id) = tile.artifact_id {
                self.pending_edit_source = None;
                if self.selected_artifact.is_some() {
                    if let Some(index) = self
                        .lightbox_frames
                        .iter()
                        .position(|frame| frame.id == artifact_id)
                    {
                        self.select_artifact_index(index, cx);
                    } else {
                        self.refresh_lightbox_frames();
                        if let Some(index) = self
                            .lightbox_frames
                            .iter()
                            .position(|frame| frame.id == artifact_id)
                        {
                            self.select_artifact_index(index, cx);
                        }
                    }
                }
            } else if self.selected_artifact.is_some() {
                self.refresh_lightbox_frames();
            }
        }
    }

    fn resolve_edit_model(&self, source_id: StudioArtifactId) -> Option<MediaModel> {
        let edits: Vec<_> = self.edit_models().cloned().collect();
        if let Some(last) = &self.remembered.last_edit_model_id
            && let Some(model) = edits.iter().find(|model| &model.id == last)
        {
            return Some(model.clone());
        }
        if let Some(source_model) = self.source_run_model_id(source_id) {
            let sibling = if source_model.as_str().ends_with("-edit") {
                source_model.clone()
            } else {
                ModelId::new(format!("{}-edit", source_model.as_str()))
            };
            if let Some(model) = edits.iter().find(|model| model.id == sibling) {
                return Some(model.clone());
            }
        }
        edits.into_iter().next()
    }

    fn source_run_model_id(&self, artifact_id: StudioArtifactId) -> Option<ModelId> {
        let view = self.conversation.as_ref()?;
        for turn in &view.turns {
            for run in &turn.runs {
                if run
                    .artifacts
                    .iter()
                    .any(|artifact| artifact.id == artifact_id)
                {
                    return Some(run.model.id.clone());
                }
            }
        }
        None
    }

    pub(super) fn refresh_lightbox_frames(&mut self) {
        if self.selected_artifact.is_none() {
            return;
        }
        if let Some(view) = &self.conversation {
            self.lightbox_frames = super::artifact::frames_from_conversation(view);
        }
    }

    pub(super) fn begin_edit_stroke(
        &mut self,
        position: Point<Pixels>,
        modifiers: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.edit_target.is_none() {
            return false;
        }
        if modifiers.shift {
            self.edit_space_pan = true;
            return false;
        }
        let Some(image) = self.pointer_to_image(position) else {
            return false;
        };
        let Some(paint) = self.edit_paint.as_mut() else {
            return false;
        };
        let radius = PaintSession::brush_radius(paint.width, paint.height, self.edit_brush_t);
        paint.begin_stroke(image, radius);
        cx.notify();
        true
    }

    pub(super) fn extend_edit_stroke(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(image) = self.pointer_to_image(position) else {
            return;
        };
        let min_distance = self.screen_to_image_distance(1.4);
        let Some(paint) = self.edit_paint.as_mut() else {
            return;
        };
        paint.extend_stroke_min(image, min_distance);
        cx.notify();
    }

    pub(super) fn end_edit_stroke(&mut self, cx: &mut Context<Self>) {
        if let Some(paint) = self.edit_paint.as_mut() {
            paint.end_stroke();
            cx.notify();
        }
        self.edit_space_pan = false;
    }

    pub(super) fn undo_edit_stroke(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(paint) = self.edit_paint.as_mut() else {
            return false;
        };
        if paint.undo() {
            cx.notify();
            true
        } else {
            false
        }
    }

    pub(super) fn redo_edit_stroke(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(paint) = self.edit_paint.as_mut() else {
            return false;
        };
        if paint.redo() {
            cx.notify();
            true
        } else {
            false
        }
    }

    fn screen_to_image_distance(&self, screen_px: f32) -> f32 {
        let Some(paint) = self.edit_paint.as_ref() else {
            return screen_px;
        };
        let stage = Bounds {
            origin: self.lightbox_stage_origin,
            size: size(
                px(self.lightbox_stage_width.max(1.0)),
                px(self.lightbox_stage_height.max(1.0)),
            ),
        };
        let bounds = super::artifact::lightbox_image_paint_bounds(
            stage,
            paint.width,
            paint.height,
            self.lightbox_zoom,
            self.lightbox_pan,
            0.0,
        );
        let display_w = f32::from(bounds.size.width).max(1.0);
        screen_px * (paint.width as f32 / display_w)
    }

    fn pointer_to_image(&self, position: Point<Pixels>) -> Option<(f32, f32)> {
        let paint = self.edit_paint.as_ref()?;
        let stage = Bounds {
            origin: self.lightbox_stage_origin,
            size: size(
                px(self.lightbox_stage_width.max(1.0)),
                px(self.lightbox_stage_height.max(1.0)),
            ),
        };
        let bounds = super::artifact::lightbox_image_paint_bounds(
            stage,
            paint.width,
            paint.height,
            self.lightbox_zoom,
            self.lightbox_pan,
            0.0,
        );
        if !bounds.contains(&position) {
            return None;
        }
        let nx = (f32::from(position.x - bounds.origin.x) / f32::from(bounds.size.width))
            .clamp(0.0, 1.0);
        let ny = (f32::from(position.y - bounds.origin.y) / f32::from(bounds.size.height))
            .clamp(0.0, 1.0);
        Some((nx * paint.width as f32, ny * paint.height as f32))
    }

    pub(super) fn render_edit_strokes(&self) -> AnyElement {
        let Some(paint) = self.edit_paint.as_ref() else {
            return div().into_any_element();
        };
        let width = paint.width;
        let height = paint.height;
        let strokes: Vec<super::paint::Stroke> = paint.iter_strokes().cloned().collect();
        let zoom = self.lightbox_zoom;
        let pan = self.lightbox_pan;
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let image_bounds = super::artifact::lightbox_image_paint_bounds(
                    bounds, width, height, zoom, pan, 0.0,
                );
                paint_vector_strokes(window, image_bounds, width, height, &strokes);
            },
        )
        .absolute()
        .inset_0()
        .into_any_element()
    }

    pub(super) fn render_edit_composer(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let prompt = self.edit_prompt.read(cx).text();
        let blocked = prompt.trim().is_empty() || self.busy;
        // Compact chat pill: one 22.75px line in a 49px row. A stretched
        // `h_full` well paints the placeholder at the top of empty space
        // and a well with no height collapses the input to 0 (unclickable).
        let card = div()
            .id("studio-edit-composer")
            .w_full()
            .max_w(px(560.0))
            .h(px(COMPACT_TOTAL_HEIGHT))
            .occlude()
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .when(!theme.is_glass(), |card| card.shadow_lg())
            .flex()
            .flex_row()
            .items_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, _, window, cx| {
                    window.focus(&page.edit_prompt.focus_handle(cx), cx);
                    cx.stop_propagation();
                }),
            )
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("studio-edit-prompt")
                    .flex_1()
                    .min_w_0()
                    .h(px(INPUT_LINE_HEIGHT))
                    .pl(px(16.0))
                    .pr(px(8.0))
                    .overflow_hidden()
                    .cursor(gpui::CursorStyle::IBeam)
                    .child(self.edit_prompt.clone()),
            )
            .child(
                div().flex_none().pr(px(8.0)).child(
                    div()
                        .id("studio-edit-send")
                        .size(px(28.0))
                        .flex_none()
                        .rounded_full()
                        .bg(theme.text)
                        .flex()
                        .items_center()
                        .justify_center()
                        .when(blocked, |button| button.opacity(0.35))
                        .when(!blocked, |button| {
                            button
                                .cursor_pointer()
                                .hover(|style| style.opacity(0.85))
                                .on_click(cx.listener(|page, _, _, cx| page.submit_edit(cx)))
                        })
                        .child(
                            crate::icons::icon(crate::icons::ARROW_UP)
                                .size(px(14.0))
                                .text_color(theme.bg),
                        ),
                ),
            );
        div()
            .id("studio-artifact-edit-composer")
            .absolute()
            .bottom(px(EDIT_COMPOSER_BOTTOM))
            .left_0()
            .right_0()
            .flex()
            .justify_center()
            .child(crate::frost::frosted(26.0, 16.0, card))
            .into_any_element()
    }

    pub(super) fn render_brush_slider(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let t = self.edit_brush_t;
        let ink = theme.text;
        let entity = cx.weak_entity();
        div()
            .id("studio-edit-brush")
            .absolute()
            .right(px(18.0))
            .top_0()
            .bottom_0()
            .w(px(28.0))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(8.0))
            .occlude()
            .child(brush_legend_dot(11.0, theme))
            .child(
                div()
                    .relative()
                    .w(px(22.0))
                    .h(px(BRUSH_TRACK_HEIGHT))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                            page.set_brush_from_y(f32::from(event.position.y), cx);
                            page.edit_brush_drag = true;
                        }),
                    )
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, (), window, _| {
                                paint_brush_track(bounds, t, ink, window);
                                let entity_move = entity.clone();
                                let track = bounds;
                                window.on_mouse_event(
                                    move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                                        if phase != gpui::DispatchPhase::Bubble || !event.dragging()
                                        {
                                            return;
                                        }
                                        let _ = entity_move.update(cx, |page, cx| {
                                            if page.edit_brush_drag {
                                                page.set_brush_from_track(
                                                    track,
                                                    f32::from(event.position.y),
                                                    cx,
                                                );
                                            }
                                        });
                                    },
                                );
                                let entity_up = entity.clone();
                                window.on_mouse_event(
                                    move |event: &gpui::MouseUpEvent, phase, _, cx| {
                                        if phase != gpui::DispatchPhase::Bubble
                                            || event.button != MouseButton::Left
                                        {
                                            return;
                                        }
                                        let _ = entity_up.update(cx, |page, cx| {
                                            if page.edit_brush_drag {
                                                page.edit_brush_drag = false;
                                                cx.notify();
                                            }
                                        });
                                    },
                                );
                            },
                        )
                        .size_full(),
                    ),
            )
            .child(brush_legend_dot(6.0, theme))
            .into_any_element()
    }

    fn set_brush_from_y(&mut self, y: f32, cx: &mut Context<Self>) {
        // Approximate until the canvas reports bounds via drag.
        let _ = y;
        cx.notify();
    }

    fn set_brush_from_track(&mut self, track: Bounds<Pixels>, y: f32, cx: &mut Context<Self>) {
        let top = f32::from(track.origin.y);
        let height = f32::from(track.size.height).max(1.0);
        let t = 1.0 - ((y - top) / height).clamp(0.0, 1.0);
        self.edit_brush_t = t;
        cx.notify();
    }

    pub(super) fn render_edit_action(
        &self,
        artifact_id: StudioArtifactId,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let available = self.edit_is_available();
        let active = self.edit_target == Some(artifact_id);
        let mut button = div()
            .id("studio-edit-artifact")
            .h(px(34.0))
            .w_full()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(7.0))
            .rounded(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM);
        if !available {
            return button
                .bg(crate::theme::wash(0.06))
                .text_color(theme.text_faint)
                .opacity(0.58)
                .child("Edit image")
                .into_any_element();
        }
        if active {
            button = button
                .bg(theme.text)
                .text_color(theme.on_solid)
                .cursor_pointer()
                .hover(|style| style.opacity(0.88))
                .on_click(cx.listener(move |page, _, _, cx| page.exit_edit_mode(cx)))
                .child("Editing");
        } else {
            button = button
                .bg(theme.text)
                .text_color(theme.on_solid)
                .cursor_pointer()
                .hover(|style| style.opacity(0.88))
                .on_click(cx.listener(move |page, _, window, cx| {
                    page.enter_edit_mode(artifact_id, window, cx);
                }))
                .child(
                    crate::icons::icon(crate::icons::PEN)
                        .size(px(13.0))
                        .text_color(theme.on_solid),
                )
                .child("Edit image");
        }
        button.into_any_element()
    }
}

fn brush_legend_dot(size_px: f32, theme: &Theme) -> AnyElement {
    div()
        .size(px(size_px))
        .rounded_full()
        .bg(theme.text)
        .into_any_element()
}

fn paint_brush_track(bounds: Bounds<Pixels>, t: f32, ink: gpui::Hsla, window: &mut Window) {
    let x = bounds.origin.x + bounds.size.width / 2.0;
    let top = bounds.origin.y;
    let bottom = bounds.origin.y + bounds.size.height;
    let top_half = px(8.0);
    let bottom_half = px(2.5);
    let mut fill = PathBuilder::fill();
    fill.move_to(point(x - top_half, top + px(6.0)));
    fill.line_to(point(x + top_half, top + px(6.0)));
    fill.line_to(point(x + bottom_half, bottom - px(6.0)));
    fill.line_to(point(x - bottom_half, bottom - px(6.0)));
    if let Ok(path) = fill.build() {
        window.paint_path(path, ink.opacity(0.55));
    }
    let mut outline = PathBuilder::stroke(px(1.5));
    outline.move_to(point(x - top_half, top + px(6.0)));
    outline.line_to(point(x + top_half, top + px(6.0)));
    outline.line_to(point(x + bottom_half, bottom - px(6.0)));
    outline.line_to(point(x - bottom_half, bottom - px(6.0)));
    outline.line_to(point(x - top_half, top + px(6.0)));
    if let Ok(path) = outline.build() {
        window.paint_path(path, ink);
    }
    let thumb_y = top + px(6.0) + (bounds.size.height - px(12.0)) * (1.0 - t.clamp(0.0, 1.0));
    window.paint_quad(gpui::quad(
        Bounds {
            origin: point(x - px(8.0), thumb_y - px(6.0)),
            size: size(px(16.0), px(12.0)),
        },
        px(6.0),
        ink,
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
}

fn paint_vector_strokes(
    window: &mut Window,
    image_bounds: Bounds<Pixels>,
    image_width: u32,
    image_height: u32,
    strokes: &[super::paint::Stroke],
) {
    let scale_x = f32::from(image_bounds.size.width) / image_width.max(1) as f32;
    let scale_y = f32::from(image_bounds.size.height) / image_height.max(1) as f32;
    let to_screen = |x: f32, y: f32| {
        point(
            image_bounds.origin.x + px(x * scale_x),
            image_bounds.origin.y + px(y * scale_y),
        )
    };
    let fill = gpui::hsla(0.0, 0.0, 1.0, 0.22);
    let outline = gpui::hsla(0.0, 0.0, 1.0, 0.95);
    for stroke in strokes {
        if stroke.points.is_empty() {
            continue;
        }
        let radius_px = stroke.radius * scale_x;
        let fill_width = (radius_px * 2.0).max(2.0);
        let outline_width = fill_width + 3.0;
        paint_smooth_stroke(window, stroke, to_screen, px(outline_width), outline);
        paint_smooth_stroke(window, stroke, to_screen, px(fill_width), fill);
        let Some(&(start_x, start_y)) = stroke.points.first() else {
            continue;
        };
        let (end_x, end_y) = stroke.points.last().copied().unwrap_or((start_x, start_y));
        paint_round_cap(
            window,
            to_screen(start_x, start_y),
            outline_width * 0.5,
            outline,
        );
        paint_round_cap(window, to_screen(start_x, start_y), fill_width * 0.5, fill);
        if stroke.points.len() > 1 {
            paint_round_cap(
                window,
                to_screen(end_x, end_y),
                outline_width * 0.5,
                outline,
            );
            paint_round_cap(window, to_screen(end_x, end_y), fill_width * 0.5, fill);
        }
    }
}

fn paint_smooth_stroke(
    window: &mut Window,
    stroke: &super::paint::Stroke,
    to_screen: impl Fn(f32, f32) -> Point<Pixels>,
    width: Pixels,
    color: gpui::Hsla,
) {
    if stroke.points.len() < 2 {
        return;
    }
    let mut builder = PathBuilder::stroke(width);
    let screen: Vec<_> = stroke
        .points
        .iter()
        .map(|&(x, y)| to_screen(x, y))
        .collect();
    if screen.len() == 2 {
        builder.move_to(screen[0]);
        builder.line_to(screen[1]);
    } else {
        builder.move_to(screen[0]);
        for i in 0..screen.len() - 1 {
            let p0 = screen[i.saturating_sub(1)];
            let p1 = screen[i];
            let p2 = screen[i + 1];
            let p3 = screen[(i + 2).min(screen.len() - 1)];
            let c1 = point(p1.x + (p2.x - p0.x) / 6.0, p1.y + (p2.y - p0.y) / 6.0);
            let c2 = point(p2.x - (p3.x - p1.x) / 6.0, p2.y - (p3.y - p1.y) / 6.0);
            builder.cubic_bezier_to(p2, c1, c2);
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

fn paint_round_cap(window: &mut Window, center: Point<Pixels>, radius: f32, color: gpui::Hsla) {
    let radius = radius.max(1.0);
    window.paint_quad(gpui::quad(
        Bounds {
            origin: point(center.x - px(radius), center.y - px(radius)),
            size: size(px(radius * 2.0), px(radius * 2.0)),
        },
        px(radius),
        color,
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
}
