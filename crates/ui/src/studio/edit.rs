//! Lightbox image-edit mode: slim composer, brush, and derived-run submit.

use gpui::{
    AnyElement, App, Bounds, Context, Focusable as _, MouseButton, Pixels, Point, SharedString,
    Window, canvas, div, point, prelude::*, px, size,
};
use zeron_proto::StudioConversationView;
use zeron_rpc::methods;
use zeron_studio::{MediaKind, MediaModel, MediaOperation, ModelId, StudioArtifactId};

use crate::composer::{COMPACT_TOTAL_HEIGHT, INPUT_LINE_HEIGHT};
use crate::popover;
use crate::theme::Theme;

use super::page::StudioPage;
use super::paint::{BrushMode, PaintSession};

/// Fade band under the floating pill: compact chat height plus bottom inset.
const EDIT_COMPOSER_BOTTOM: f32 = 10.0;
pub(super) const EDIT_COMPOSER_HEIGHT: f32 = COMPACT_TOTAL_HEIGHT + EDIT_COMPOSER_BOTTOM;
const DEFAULT_BRUSH_T: f32 = 0.28;

impl StudioPage {
    pub(super) fn edit_models(&self) -> impl Iterator<Item = &MediaModel> {
        self.edit_models.iter().filter(|model| {
            model.operation == MediaOperation::ImageEdit && model.output_kind == MediaKind::Image
        })
    }

    pub(super) fn edit_is_available(&self) -> bool {
        self.edit_models().next().is_some()
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
        self.edit_add = true;
        self.edit_space_pan = false;
        self.reset_lightbox_swipe();
        self.lightbox_swipe_locked = false;
        self.edit_prompt.update(cx, |input, cx| {
            input.set_text(String::new(), cx);
        });
        window.focus(&self.edit_prompt.focus_handle(cx), cx);
        cx.notify();
    }

    pub(super) fn exit_edit_mode(&mut self, cx: &mut Context<Self>) {
        self.close_edit_model_picker(cx);
        self.edit_target = None;
        self.edit_paint = None;
        self.edit_space_pan = false;
        self.edit_brush_drag = false;
        self.edit_brush_track = None;
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
        let Some(model) = self.selected_edit_model() else {
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
                match result {
                    Ok(value) => match serde_json::from_value::<StudioConversationView>(value) {
                        Ok(view) => {
                            if page.selected_conversation == Some(conversation_id) {
                                page.conversation = Some(view.clone());
                                page.select_pending_derived(&view, source_id, cx);
                            }
                        }
                        Err(error) => {
                            page.pending_edit_source = None;
                            page.error = Some(format!("Edit response was invalid: {error}").into());
                        }
                    },
                    Err(error) => {
                        page.pending_edit_source = None;
                        page.error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    pub(super) fn select_pending_derived(
        &mut self,
        view: &StudioConversationView,
        source_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let child = super::lineage::lineage_tiles(view)
            .into_iter()
            .rev()
            .find(|tile| tile.source_artifact_id == Some(source_id));
        let Some(tile) = child else {
            return;
        };
        self.refresh_lightbox_frames(cx);
        let key = match tile.artifact_id {
            Some(artifact_id) => super::artifact::ArtifactFrameKey::Ready(artifact_id),
            None => super::artifact::ArtifactFrameKey::Loading {
                run_id: tile.run_id,
                output_ix: tile.output_ix,
            },
        };
        if tile.artifact_id.is_some() {
            self.pending_edit_source = None;
        }
        let stay_on_source =
            self.selected_frame == Some(super::artifact::ArtifactFrameKey::Ready(source_id));
        let already_on_child = self.selected_frame == Some(key);
        if !stay_on_source && !already_on_child && self.selected_frame.is_some() {
            return;
        }
        if let Some(index) = self
            .lightbox_frames
            .iter()
            .position(|frame| frame.key == key)
        {
            self.select_artifact_index(index, cx);
        }
    }

    pub(super) fn selected_edit_model(&self) -> Option<MediaModel> {
        let edits: Vec<_> = self.edit_models().cloned().collect();
        let source_id = self.edit_target;
        preferred_edit_model(
            &edits,
            self.remembered.last_edit_model_id.as_ref(),
            source_id
                .and_then(|id| self.source_run_model_id(id))
                .as_ref(),
        )
    }

    fn select_edit_model(&mut self, id: ModelId, cx: &mut Context<Self>) {
        if self.edit_models().any(|model| model.id == id) {
            self.remembered.last_edit_model_id = Some(id);
            self.persist_composer_defaults(cx);
        }
        self.close_edit_model_picker(cx);
        cx.notify();
    }

    pub(super) fn close_edit_model_picker(&mut self, cx: &mut Context<Self>) {
        if self.edit_model_picker.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.edit_model_picker);
            cx.notify();
        }
    }

    pub(super) fn dismiss_edit_model_picker(
        &mut self,
        event: &gpui::KeyDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if event.keystroke.key == "escape" && self.edit_model_picker.is_open() {
            self.close_edit_model_picker(cx);
            true
        } else {
            false
        }
    }

    fn toggle_edit_model_picker(&mut self, cx: &mut Context<Self>) {
        let pressed_open = self.edit_model_picker.take_press_was_open();
        if self.edit_model_picker.is_open() || pressed_open {
            self.close_edit_model_picker(cx);
            return;
        }
        self.edit_model_picker.open(());
        cx.notify();
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

    pub(super) fn refresh_lightbox_frames(&mut self, cx: &mut Context<Self>) {
        let Some(previous) = self.selected_frame else {
            return;
        };
        if let Some(view) = &self.conversation {
            self.lightbox_frames = super::artifact::frames_from_conversation(view);
        }
        if let Some(resolved) = super::artifact::resolve_frame_key(previous, &self.lightbox_frames)
        {
            self.selected_frame = Some(resolved);
            if previous.artifact_id().is_none()
                && let Some(artifact_id) = resolved.artifact_id()
                && let Some(conversation_id) = self.artifact_conversation(artifact_id)
            {
                cx.emit(super::StudioEvent::OpenArtifact {
                    conversation_id,
                    artifact_id,
                });
            }
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
        let mode = if self.edit_add {
            BrushMode::Add
        } else {
            BrushMode::Subtract
        };
        paint.begin_stroke(image, radius, mode);
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

    pub(super) fn invert_edit_mask(&mut self, cx: &mut Context<Self>) {
        if let Some(paint) = self.edit_paint.as_mut() {
            paint.invert();
            cx.notify();
        }
    }

    pub(super) fn reset_edit_mask(&mut self, cx: &mut Context<Self>) {
        if let Some(paint) = self.edit_paint.as_mut() {
            paint.reset();
            cx.notify();
        }
    }

    pub(super) fn render_edit_strokes(&self, window: &mut Window, cx: &mut App) -> AnyElement {
        let Some(paint) = self.edit_paint.as_ref() else {
            return div().into_any_element();
        };
        paint.flush_stale_gpu(window, cx);
        let Some(gpu) = paint.overlay_gpu() else {
            return div().into_any_element();
        };
        let width = paint.width;
        let height = paint.height;
        let zoom = self.lightbox_zoom;
        let pan = self.lightbox_pan;
        canvas(
            |_, _, _| {},
            move |bounds, (), window, _| {
                let image_bounds = super::artifact::lightbox_image_paint_bounds(
                    bounds, width, height, zoom, pan, 0.0,
                );
                let scale_x = f32::from(image_bounds.size.width) / width.max(1) as f32;
                let scale_y = f32::from(image_bounds.size.height) / height.max(1) as f32;
                let dest = Bounds {
                    origin: point(
                        image_bounds.origin.x + px(gpu.x0 as f32 * scale_x),
                        image_bounds.origin.y + px(gpu.y0 as f32 * scale_y),
                    ),
                    size: size(
                        px((gpu.x1 - gpu.x0 + 1) as f32 * scale_x),
                        px((gpu.y1 - gpu.y0 + 1) as f32 * scale_y),
                    ),
                };
                window
                    .paint_image(dest, gpui::Corners::default(), gpu.image, 0, false)
                    .ok();
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
                div()
                    .flex_none()
                    .pr(px(8.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(6.0))
                    .child(self.render_edit_model_chip(theme, cx))
                    .child(
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

    fn render_edit_model_chip(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let selected = self.selected_edit_model();
        let label = selected
            .as_ref()
            .map(|model| model.display_name.clone())
            .unwrap_or_else(|| "Model".into());
        let selected_id = selected.as_ref().map(|model| model.id.clone());
        let open = self.edit_model_picker.is_open() || self.edit_model_picker.is_closing();
        let menu = open.then(|| {
            let mut card = popover::popover_card(theme)
                .id("studio-edit-model-picker")
                .min_w(px(200.0))
                .max_w(px(280.0))
                .on_mouse_down_out(cx.listener(|page, _, _, cx| {
                    page.close_edit_model_picker(cx);
                }))
                .flex()
                .flex_col()
                .gap(px(2.0));
            for model in self.edit_models() {
                let id = model.id.clone();
                let active = selected_id.as_ref() == Some(&id);
                let fade = format!("studio-edit-model-{}", id.as_str());
                let mut row = popover::menu_row(theme, active, fade.clone())
                    .id(SharedString::from(fade))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.select_edit_model(id.clone(), cx);
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(model.display_name.clone())),
                    );
                if active {
                    row = row.child(
                        crate::icons::icon(crate::icons::CHECK)
                            .size(px(13.0))
                            .text_color(theme.text_muted),
                    );
                }
                card = card.child(row);
            }
            popover::anchored_menu_above_end(
                "studio-edit-model-menu",
                card.into_any_element(),
                self.edit_model_picker.closing_since(),
            )
        });
        let mut chip = div()
            .id("studio-edit-model")
            .relative()
            .h(px(28.0))
            .max_w(px(148.0))
            .min_w_0()
            .flex()
            .flex_row()
            .items_center()
            .px(px(8.0))
            .rounded(px(8.0))
            .text_size(px(12.0))
            .font_weight(gpui::FontWeight::MEDIUM)
            .text_color(theme.text.opacity(0.9))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.10)))
            .when(open, |chip| chip.bg(crate::theme::wash(0.10)))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                    page.edit_model_picker.note_trigger_press();
                    cx.stop_propagation();
                    let _ = event;
                }),
            )
            .on_click(cx.listener(|page, _, _, cx| {
                cx.stop_propagation();
                page.toggle_edit_model_picker(cx);
            }))
            .child(div().min_w_0().truncate().child(SharedString::from(label)));
        if let Some(menu) = menu {
            chip = chip.child(menu);
        }
        chip.into_any_element()
    }

    pub(super) fn render_precise_edit_sidebar(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let adding = self.edit_add;
        div()
            .id("studio-precise-edit")
            .w_full()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .occlude()
            .child(
                div()
                    .w_full()
                    .h(px(32.0))
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .id("studio-precise-edit-close")
                            .size(px(28.0))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(8.0))
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::wash(0.10)))
                            .on_click(cx.listener(|page, _, _, cx| page.exit_edit_mode(cx)))
                            .child(
                                crate::icons::icon(crate::icons::CLOSE)
                                    .size(px(14.0))
                                    .text_color(theme.text),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .text_size(px(15.0))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Precise Edit"),
                    )
                    .child(div().size(px(28.0)).flex_none()),
            )
            .child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.text_muted)
                    .child("Brush size"),
            )
            .child(self.render_brush_size_slider(theme, cx))
            .child(
                precise_edit_button(
                    "studio-precise-edit-mode",
                    if adding {
                        crate::icons::ADD_CIRCLE
                    } else {
                        crate::icons::MINUS_CIRCLE
                    },
                    if adding {
                        "Add to selection"
                    } else {
                        "Remove from selection"
                    },
                    true,
                    theme,
                )
                .on_click(cx.listener(|page, _, _, cx| {
                    page.edit_add = !page.edit_add;
                    cx.notify();
                })),
            )
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_row()
                    .gap(px(8.0))
                    .child(
                        precise_edit_button(
                            "studio-precise-edit-invert",
                            crate::icons::INVERT,
                            "Invert",
                            false,
                            theme,
                        )
                        .flex_1()
                        .on_click(cx.listener(|page, _, _, cx| page.invert_edit_mask(cx))),
                    )
                    .child(
                        precise_edit_button(
                            "studio-precise-edit-reset",
                            crate::icons::RESTART,
                            "Reset",
                            false,
                            theme,
                        )
                        .flex_1()
                        .on_click(cx.listener(|page, _, _, cx| page.reset_edit_mask(cx))),
                    ),
            )
            .into_any_element()
    }

    fn render_brush_size_slider(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let t = self.edit_brush_t;
        let ink = theme.text;
        let entity = cx.weak_entity();
        div()
            .id("studio-edit-brush")
            .w_full()
            .h(px(44.0))
            .rounded(px(12.0))
            .bg(crate::theme::wash(0.08))
            .px(px(12.0))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(10.0))
            .child(brush_legend_ring(7.0, theme))
            .child(
                div()
                    .id("studio-edit-brush-track")
                    .relative()
                    .flex_1()
                    .h(px(24.0))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                            page.edit_brush_drag = true;
                            if let Some(track) = page.edit_brush_track {
                                page.set_brush_from_track(track, f32::from(event.position.x), cx);
                            }
                            cx.stop_propagation();
                        }),
                    )
                    .child(
                        canvas(
                            |_, _, _| {},
                            move |bounds, (), window, cx| {
                                let _ = entity.update(cx, |page, _| {
                                    page.edit_brush_track = Some(bounds);
                                });
                                paint_brush_size_track(bounds, t, ink, window);
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
                                                    f32::from(event.position.x),
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
            .child(brush_legend_ring(14.0, theme))
            .into_any_element()
    }

    fn set_brush_from_track(&mut self, track: Bounds<Pixels>, x: f32, cx: &mut Context<Self>) {
        let left = f32::from(track.origin.x);
        let width = f32::from(track.size.width).max(1.0);
        self.edit_brush_t = ((x - left) / width).clamp(0.0, 1.0);
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

fn precise_edit_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    center_label: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(44.0))
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .gap(px(10.0))
        .px(px(14.0))
        .rounded(px(12.0))
        .bg(crate::theme::wash(0.08))
        .cursor_pointer()
        .hover(|style| style.bg(crate::theme::wash(0.12)))
        .child(
            crate::icons::icon(icon)
                .size(px(16.0))
                .text_color(theme.text),
        )
        .child(
            div()
                .flex_1()
                .when(center_label, |label_el| label_el.flex().justify_center())
                .text_size(px(14.0))
                .font_weight(gpui::FontWeight::MEDIUM)
                .text_color(theme.text)
                .child(SharedString::from(label)),
        )
}

fn brush_legend_ring(size_px: f32, theme: &Theme) -> AnyElement {
    div()
        .size(px(size_px))
        .rounded_full()
        .border_1()
        .border_color(theme.text.opacity(0.7))
        .flex_none()
        .into_any_element()
}

fn preferred_edit_model(
    edits: &[MediaModel],
    last_id: Option<&ModelId>,
    source_model_id: Option<&ModelId>,
) -> Option<MediaModel> {
    if let Some(last) = last_id
        && let Some(model) = edits.iter().find(|model| &model.id == last)
    {
        return Some(model.clone());
    }
    if let Some(source_model) = source_model_id {
        let sibling = if source_model.as_str().ends_with("-edit") {
            source_model.clone()
        } else {
            ModelId::new(format!("{}-edit", source_model.as_str()))
        };
        if let Some(model) = edits.iter().find(|model| model.id == sibling) {
            return Some(model.clone());
        }
    }
    edits.first().cloned()
}

fn paint_brush_size_track(bounds: Bounds<Pixels>, t: f32, ink: gpui::Hsla, window: &mut Window) {
    let mid_y = bounds.origin.y + bounds.size.height / 2.0;
    let left = bounds.origin.x + px(8.0);
    let right = bounds.origin.x + bounds.size.width - px(8.0);
    let width = right - left;
    window.paint_quad(gpui::quad(
        Bounds {
            origin: point(left, mid_y - px(0.75)),
            size: size(width, px(1.5)),
        },
        px(1.0),
        ink.opacity(0.35),
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
    let thumb_x = left + width * t.clamp(0.0, 1.0);
    window.paint_quad(gpui::quad(
        Bounds {
            origin: point(thumb_x - px(7.0), mid_y - px(7.0)),
            size: size(px(14.0), px(14.0)),
        },
        px(7.0),
        ink,
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zeron_studio::MediaKind;

    fn edit_model(id: &str, name: &str) -> MediaModel {
        MediaModel {
            provider_id: "venice".into(),
            id: id.into(),
            display_name: name.into(),
            description: None,
            operation: MediaOperation::ImageEdit,
            output_kind: MediaKind::Image,
            output_mime_types: vec!["image/png".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls: Vec::new(),
            pricing: None,
            features: Vec::new(),
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn edit_model_prefers_last_used_then_sibling_then_first() {
        let qwen = edit_model("qwen-edit", "Qwen Edit");
        let flux = edit_model("flux-edit", "Flux Edit");
        let edits = vec![qwen.clone(), flux.clone()];
        assert_eq!(
            preferred_edit_model(&edits, Some(&flux.id), None)
                .unwrap()
                .id,
            flux.id
        );
        assert_eq!(
            preferred_edit_model(&edits, None, Some(&ModelId::new("qwen")))
                .unwrap()
                .id,
            qwen.id
        );
        assert_eq!(
            preferred_edit_model(&edits, None, None).unwrap().id,
            qwen.id
        );
        assert_eq!(
            preferred_edit_model(&edits, Some(&ModelId::new("gone")), None)
                .unwrap()
                .id,
            qwen.id
        );
    }
}
