//! Lightbox image-edit mode: slim composer, brush, and derived-run submit.

use gpui::{
    AnyElement, Bounds, Context, Focusable as _, MouseButton, PathBuilder, Pixels, Point,
    SharedString, Window, canvas, div, point, prelude::*, px, size,
};
use zeron_proto::StudioConversationView;
use zeron_rpc::methods;
use zeron_studio::{MediaKind, MediaModel, MediaOperation, ModelId, StudioArtifactId};

use crate::theme::Theme;

use super::page::StudioPage;
use super::paint::PaintSession;

pub(super) const EDIT_COMPOSER_HEIGHT: f32 = 96.0;
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
        self.edit_overlay = None;
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
        self.edit_overlay = None;
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
        let display_aspect_ratio = self
            .artifact_frame(source_id)
            .and_then(|frame| frame.width.zip(frame.height))
            .filter(|(width, height)| *width > 0 && *height > 0)
            .unwrap_or((1, 1));
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
            return true;
        };
        let Some(paint) = self.edit_paint.as_mut() else {
            return true;
        };
        let radius = PaintSession::brush_radius(paint.width, paint.height, self.edit_brush_t);
        paint.begin_stroke(image, radius);
        self.sync_edit_overlay();
        cx.notify();
        true
    }

    pub(super) fn extend_edit_stroke(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(image) = self.pointer_to_image(position) else {
            return;
        };
        let Some(paint) = self.edit_paint.as_mut() else {
            return;
        };
        paint.extend_stroke(image);
        self.sync_edit_overlay();
        cx.notify();
    }

    pub(super) fn end_edit_stroke(&mut self, cx: &mut Context<Self>) {
        if let Some(paint) = self.edit_paint.as_mut() {
            paint.end_stroke();
            self.sync_edit_overlay();
            cx.notify();
        }
        self.edit_space_pan = false;
    }

    pub(super) fn undo_edit_stroke(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(paint) = self.edit_paint.as_mut() else {
            return false;
        };
        if paint.undo() {
            self.sync_edit_overlay();
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
            self.sync_edit_overlay();
            cx.notify();
            true
        } else {
            false
        }
    }

    fn sync_edit_overlay(&mut self) {
        self.edit_overlay = self
            .edit_paint
            .as_ref()
            .and_then(PaintSession::overlay_image);
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

    pub(super) fn render_edit_overlay(
        &self,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let _ = (theme, window, cx);
        let overlay = self.edit_overlay.clone()?;
        let paint = self.edit_paint.as_ref()?;
        Some(
            super::artifact::contain_layers(
                overlay,
                None,
                px(0.0),
                Some(SharedString::from(format!(
                    "studio-edit-overlay-{}-{}",
                    paint.width, paint.height
                ))),
            )
            .into_any_element(),
        )
    }

    pub(super) fn render_edit_composer(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let prompt = self.edit_prompt.read(cx).text();
        let blocked = prompt.trim().is_empty() || self.busy;
        let card = div()
            .id("studio-edit-composer")
            .w_full()
            .max_w(px(560.0))
            .occlude()
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .when(!theme.is_glass(), |card| card.shadow_lg())
            .px(px(8.0))
            .pt(px(8.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .child(
                div()
                    .relative()
                    .w_full()
                    .min_h(px(52.0))
                    .child(
                        div()
                            .id("studio-edit-prompt")
                            .w_full()
                            .pr(px(40.0))
                            .child(self.edit_prompt.clone()),
                    )
                    .child(
                        div()
                            .absolute()
                            .right(px(4.0))
                            .bottom(px(4.0))
                            .id("studio-edit-send")
                            .size(px(28.0))
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
            .bottom_0()
            .left_0()
            .right_0()
            .h(px(EDIT_COMPOSER_HEIGHT))
            .flex()
            .justify_center()
            .items_end()
            .pb(px(12.0))
            .occlude()
            .child(crate::frost::frosted(26.0, 16.0, card))
            .into_any_element()
    }

    pub(super) fn render_brush_slider(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let t = self.edit_brush_t;
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
            .child(brush_legend_dot(10.0, theme))
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
                                paint_brush_track(bounds, t, window);
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
            .child(brush_legend_dot(5.0, theme))
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
        .bg(theme.text.opacity(0.85))
        .into_any_element()
}

fn paint_brush_track(bounds: Bounds<Pixels>, t: f32, window: &mut Window) {
    let theme_white = gpui::hsla(0.0, 0.0, 1.0, 0.88);
    let track = gpui::hsla(0.0, 0.0, 1.0, 0.22);
    let x = bounds.origin.x + bounds.size.width / 2.0;
    let top = bounds.origin.y;
    let bottom = bounds.origin.y + bounds.size.height;
    let top_half = px(7.0);
    let bottom_half = px(2.0);
    let mut path = PathBuilder::fill();
    path.move_to(point(x - top_half, top + px(6.0)));
    path.line_to(point(x + top_half, top + px(6.0)));
    path.line_to(point(x + bottom_half, bottom - px(6.0)));
    path.line_to(point(x - bottom_half, bottom - px(6.0)));
    if let Ok(path) = path.build() {
        window.paint_path(path, track);
    }
    let thumb_y = top + px(6.0) + (bounds.size.height - px(12.0)) * (1.0 - t.clamp(0.0, 1.0));
    window.paint_quad(gpui::fill(
        Bounds {
            origin: point(x - px(7.0), thumb_y - px(5.0)),
            size: size(px(14.0), px(10.0)),
        },
        theme_white,
    ));
}
