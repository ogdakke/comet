//! Overlay scrollbar used by the studio feed and the agent chat transcript.
//!
//! gpui's `overflow_*_scroll` / `list` only move content — they do not paint a
//! thumb. This primitive is the shared chrome: a thin right-edge overlay that
//! reads live scroll metrics, brightens and thickens on hover, and is dragged
//! to scrub. The track is unpainted; only the thumb is ink, so a theme flip
//! (`appearance` + `refresh_windows`) recolors it on the next frame.

use std::rc::Rc;

use gpui::{
    App, Bounds, DispatchPhase, ElementId, Entity, InteractiveElement as _, IntoElement, ListState,
    MouseButton, MouseMoveEvent, MouseUpEvent, Pixels, RenderOnce, ScrollHandle, SharedString,
    Window, canvas, div, point, prelude::*, px,
};

use crate::motion;
use crate::theme;

/// Hit-strip width. The thumb sits inside it, right-aligned, so hover can
/// grow the thumb leftward without leaving the window.
pub const TRACK_WIDTH: f32 = 12.0;
/// Thumb thickness at rest.
pub const THUMB_WIDTH_REST: f32 = 4.0;
/// Thumb thickness while hovered or dragged.
pub const THUMB_WIDTH_HOVER: f32 = 7.0;
/// Floor so a long document still has something to grab.
pub const MIN_THUMB: f32 = 36.0;
/// Air between the thumb and the track's top/bottom.
pub const TRACK_PAD: f32 = 4.0;
/// Gap from the window's right edge.
pub const TRACK_INSET_END: f32 = 3.0;

/// Dark-mode thumb alpha at rest. Light flips the ink automatically.
pub const THUMB_REST_ALPHA: f32 = 0.20;
/// Dark-mode thumb alpha on hover / while dragging.
pub const THUMB_HOVER_ALPHA: f32 = 0.44;

/// Live metrics of a scrollable — pixels, y-down, `offset` in `[0, max]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScrollMetrics {
    pub offset: f32,
    pub max_offset: f32,
    pub viewport: Bounds<Pixels>,
}

/// Anything the overlay can drive. Studio uses a [`ScrollHandle`]; the agent
/// chat uses a virtualized [`ListState`].
#[derive(Clone)]
pub enum ScrollSource {
    Handle(ScrollHandle),
    List(ListState),
}

impl From<&ScrollHandle> for ScrollSource {
    fn from(handle: &ScrollHandle) -> Self {
        Self::Handle(handle.clone())
    }
}

impl From<ScrollHandle> for ScrollSource {
    fn from(handle: ScrollHandle) -> Self {
        Self::Handle(handle)
    }
}

impl From<&ListState> for ScrollSource {
    fn from(list: &ListState) -> Self {
        Self::List(list.clone())
    }
}

impl From<ListState> for ScrollSource {
    fn from(list: ListState) -> Self {
        Self::List(list)
    }
}

impl ScrollSource {
    /// Measured viewport / content from the last layout. Never estimated —
    /// unmeasured list rows must be sized by [`ListState::measure_all`].
    pub fn metrics(&self) -> Option<ScrollMetrics> {
        let (offset_y, max_y, viewport) = match self {
            Self::Handle(handle) => (
                -f32::from(handle.offset().y),
                f32::from(handle.max_offset().y),
                handle.bounds(),
            ),
            Self::List(list) => (
                -f32::from(list.scroll_px_offset_for_scrollbar().y),
                f32::from(list.max_offset_for_scrollbar().y),
                list.viewport_bounds(),
            ),
        };
        if f32::from(viewport.size.height) <= 0.0 {
            return None;
        }
        Some(ScrollMetrics {
            offset: offset_y.max(0.0),
            max_offset: max_y.max(0.0),
            viewport,
        })
    }

    fn set_offset_y(&self, offset_y: f32) {
        match self {
            Self::Handle(handle) => {
                let mut point = handle.offset();
                point.y = px(offset_y);
                handle.set_offset(point);
            }
            Self::List(list) => {
                list.set_offset_from_scrollbar(point(px(0.0), px(offset_y)));
            }
        }
    }

    fn begin_drag(&self) {
        if let Self::List(list) = self {
            list.scrollbar_drag_started();
        }
    }

    fn end_drag(&self) {
        if let Self::List(list) = self {
            list.scrollbar_drag_ended();
        }
    }
}

/// Where the thumb sits inside the track, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThumbGeom {
    pub track_top: f32,
    pub track_height: f32,
    pub thumb_top: f32,
    pub thumb_height: f32,
    pub progress: f32,
}

/// Map scroll metrics onto a thumb. `None` when there is nothing to scroll
/// or the track is too short to host the minimum thumb.
pub fn thumb_geom(
    viewport_top: f32,
    viewport_height: f32,
    inset_top: f32,
    inset_bottom: f32,
    offset: f32,
    max_offset: f32,
) -> Option<ThumbGeom> {
    if max_offset <= 1.0 || viewport_height <= 0.0 {
        return None;
    }
    let track_top = viewport_top + inset_top;
    let track_height = (viewport_height - inset_top - inset_bottom).max(0.0);
    let usable = track_height - TRACK_PAD * 2.0;
    if usable < MIN_THUMB {
        return None;
    }
    let content = viewport_height + max_offset;
    let thumb_height = (usable * (viewport_height / content)).clamp(MIN_THUMB, usable);
    let travel = (usable - thumb_height).max(0.0);
    let progress = if max_offset <= 0.0 {
        0.0
    } else {
        (offset / max_offset).clamp(0.0, 1.0)
    };
    Some(ThumbGeom {
        track_top,
        track_height,
        thumb_top: track_top + TRACK_PAD + travel * progress,
        thumb_height,
        progress,
    })
}

/// Scroll offset (0 = top) for a pointer at `mouse_y`, keeping the grab
/// point inside the thumb so a drag does not jump.
pub fn offset_for_mouse(mouse_y: f32, grab: f32, geom: ThumbGeom, max_offset: f32) -> f32 {
    let usable = geom.track_height - TRACK_PAD * 2.0;
    let travel = (usable - geom.thumb_height).max(0.0);
    if travel <= 0.0 || max_offset <= 0.0 {
        return 0.0;
    }
    let t = ((mouse_y - grab - geom.track_top - TRACK_PAD) / travel).clamp(0.0, 1.0);
    t * max_offset
}

/// Thumb colors for the current appearance. Re-read every frame —
/// these go through [`theme::ink`] so they flip with the palette.
pub fn thumb_colors() -> (gpui::Hsla, gpui::Hsla) {
    (theme::ink(THUMB_REST_ALPHA), theme::ink(THUMB_HOVER_ALPHA))
}

/// Overlay scrollbar. Mount as a later sibling of the scroll container
/// inside a `relative` parent so it does not take layout and is not faded
/// by [`crate::edge_fade`].
pub fn overlay(id: impl Into<SharedString>, source: impl Into<ScrollSource>) -> OverlayScrollbar {
    OverlayScrollbar {
        id: id.into(),
        source: source.into(),
        inset_top: 0.0,
        inset_bottom: 0.0,
        on_scrub: None,
    }
}

#[derive(IntoElement)]
pub struct OverlayScrollbar {
    id: SharedString,
    source: ScrollSource,
    inset_top: f32,
    inset_bottom: f32,
    on_scrub: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
}

impl OverlayScrollbar {
    /// Shrink the track from the parent's top — keep the thumb out of the
    /// titlebar / fade band.
    pub fn inset_top(mut self, px: f32) -> Self {
        self.inset_top = px;
        self
    }

    /// Shrink the track from the parent's bottom — keep the thumb above the
    /// composer / status stack.
    pub fn inset_bottom(mut self, px: f32) -> Self {
        self.inset_bottom = px;
        self
    }

    /// Fired after a user-driven offset change (drag, track jump). Wheel
    /// over the strip is left to the scroll container underneath
    /// ([`InteractiveElement::block_mouse_except_scroll`]).
    pub fn on_scrub(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_scrub = Some(Rc::new(f));
        self
    }
}

#[derive(Default)]
struct DragState {
    drag: Option<Drag>,
    /// Last layout fingerprint we notified on. Render reads the handle
    /// *before* the scroller's prepaint, so a content-size change (images
    /// landing, list items measuring) would leave a stale too-tall thumb
    /// until something else notified — typically a wheel event.
    layout: Option<(u32, u32, u32)>,
}

#[derive(Clone, Copy)]
struct Drag {
    grab: f32,
}

impl RenderOnce for OverlayScrollbar {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let OverlayScrollbar {
            id,
            source,
            inset_top,
            inset_bottom,
            on_scrub,
        } = self;
        let element_id: ElementId = id.clone().into();
        let state = window.use_keyed_state(element_id.clone(), cx, |_, _| DragState::default());
        let dragging = state.read(cx).drag.is_some();
        let hover_key = format!("scrollbar:{id}");
        let hover_t = if dragging {
            1.0
        } else {
            motion::hover_t(&hover_key)
        };

        let metrics = source.metrics();
        let geom = metrics.and_then(|m| {
            thumb_geom(
                f32::from(m.viewport.origin.y),
                f32::from(m.viewport.size.height),
                inset_top,
                inset_bottom,
                m.offset,
                m.max_offset,
            )
        });
        let overflow = geom.is_some();

        let detector = {
            let source = source.clone();
            let source_move = source.clone();
            let source_up = source.clone();
            let state = state.clone();
            let state_move = state.clone();
            let state_up = state.clone();
            let scrub_move = on_scrub.clone();
            let insets = (inset_top, inset_bottom);
            canvas(
                move |_, _, cx| {
                    let key = metrics_key(source.metrics().as_ref());
                    state.update(cx, |s, cx| {
                        if s.layout != Some(key) {
                            s.layout = Some(key);
                            cx.notify();
                        }
                    });
                },
                // Window-level, not hitbox-level: `div.on_mouse_move` only
                // fires while the pointer hovers the track, so a drag that
                // leaves the window (or even the 12px strip) would freeze.
                // macOS keeps delivering `mouseDragged:` / `mouseUp:` to the
                // capturing view after the cursor exits.
                move |_, (), window, _| {
                    window.on_mouse_event({
                        let source = source_move.clone();
                        let state = state_move.clone();
                        let on_scrub = scrub_move.clone();
                        move |event: &MouseMoveEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble || !event.dragging() {
                                return;
                            }
                            let Some(drag) = state.read(cx).drag else {
                                return;
                            };
                            apply_drag(
                                &source,
                                drag.grab,
                                f32::from(event.position.y),
                                insets.0,
                                insets.1,
                                on_scrub.as_deref(),
                                window,
                                cx,
                            );
                        }
                    });
                    window.on_mouse_event({
                        let source = source_up.clone();
                        let state = state_up.clone();
                        move |event: &MouseUpEvent, phase, window, cx| {
                            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                                return;
                            }
                            end_drag(&source, &state, window, cx);
                        }
                    });
                },
            )
            .w(px(0.0))
            .h(px(0.0))
        };

        let (rest, hover) = thumb_colors();
        let thumb_color = motion::mix(rest, hover, hover_t);
        let thumb_w = THUMB_WIDTH_REST + (THUMB_WIDTH_HOVER - THUMB_WIDTH_REST) * hover_t;

        let track = geom.map(|geom| {
            let thumb_top_in_track = geom.thumb_top - geom.track_top;
            let hover_for_listener = hover_key.clone();
            let source_down = source.clone();
            let state_down = state.clone();
            let scrub_down = on_scrub.clone();
            let insets = (inset_top, inset_bottom);

            div()
                .id(element_id)
                .absolute()
                .top(px(inset_top))
                .bottom(px(inset_bottom))
                .right_0()
                .w(px(TRACK_WIDTH))
                .block_mouse_except_scroll()
                .on_hover(motion::hover_listener(hover_for_listener))
                .on_mouse_down(MouseButton::Left, {
                    let hover_key = hover_key.clone();
                    move |event, window, cx| {
                        let Some(metrics) = source_down.metrics() else {
                            return;
                        };
                        let Some(geom) = thumb_geom(
                            f32::from(metrics.viewport.origin.y),
                            f32::from(metrics.viewport.size.height),
                            insets.0,
                            insets.1,
                            metrics.offset,
                            metrics.max_offset,
                        ) else {
                            return;
                        };
                        let mouse_y = f32::from(event.position.y);
                        let on_thumb = mouse_y >= geom.thumb_top
                            && mouse_y <= geom.thumb_top + geom.thumb_height;
                        let grab = if on_thumb {
                            mouse_y - geom.thumb_top
                        } else {
                            geom.thumb_height / 2.0
                        };
                        source_down.begin_drag();
                        motion::set_hover(&hover_key, true, motion::reduced_motion(cx));
                        state_down.update(cx, |s, cx| {
                            s.drag = Some(Drag { grab });
                            cx.notify();
                        });
                        apply_drag(
                            &source_down,
                            grab,
                            mouse_y,
                            insets.0,
                            insets.1,
                            scrub_down.as_deref(),
                            window,
                            cx,
                        );
                    }
                })
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .bottom_0()
                        .right_0()
                        .w(px(TRACK_WIDTH))
                        .child(
                            div()
                                .absolute()
                                .top(px(thumb_top_in_track))
                                .right(px(TRACK_INSET_END))
                                .w(px(thumb_w))
                                .h(px(geom.thumb_height))
                                .rounded(px(thumb_w / 2.0))
                                .bg(thumb_color),
                        ),
                )
        });

        div()
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(px(if overflow { TRACK_WIDTH } else { 0.0 }))
            .child(detector)
            .when_some(track, |el, track| el.child(track))
    }
}

/// Quantize live metrics so a post-layout canvas can tell "the thumb
/// we painted is stale" without notifying on sub-pixel jitter.
fn metrics_key(metrics: Option<&ScrollMetrics>) -> (u32, u32, u32) {
    let Some(metrics) = metrics else {
        return (0, 0, 0);
    };
    let q = |v: f32| (v * 2.0).round().max(0.0) as u32;
    (
        q(f32::from(metrics.viewport.size.height)),
        q(metrics.max_offset),
        q(metrics.offset),
    )
}

fn apply_drag(
    source: &ScrollSource,
    grab: f32,
    mouse_y: f32,
    inset_top: f32,
    inset_bottom: f32,
    on_scrub: Option<&dyn Fn(&mut Window, &mut App)>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(metrics) = source.metrics() else {
        return;
    };
    let Some(geom) = thumb_geom(
        f32::from(metrics.viewport.origin.y),
        f32::from(metrics.viewport.size.height),
        inset_top,
        inset_bottom,
        metrics.offset,
        metrics.max_offset,
    ) else {
        return;
    };
    source.set_offset_y(-offset_for_mouse(mouse_y, grab, geom, metrics.max_offset));
    if let Some(cb) = on_scrub {
        cb(window, cx);
    }
    window.refresh();
}

fn end_drag(source: &ScrollSource, state: &Entity<DragState>, window: &mut Window, cx: &mut App) {
    if state.read(cx).drag.is_none() {
        return;
    }
    source.end_drag();
    state.update(cx, |s, cx| {
        s.drag = None;
        cx.notify();
    });
    window.refresh();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{Appearance, lock_appearance, set_current_appearance};

    #[test]
    fn no_thumb_without_overflow() {
        assert!(thumb_geom(0.0, 400.0, 0.0, 0.0, 0.0, 0.0).is_none());
        assert!(thumb_geom(0.0, 400.0, 0.0, 0.0, 0.0, 1.0).is_none());
        assert!(thumb_geom(0.0, 0.0, 0.0, 0.0, 0.0, 200.0).is_none());
    }

    #[test]
    fn thumb_sits_at_ends() {
        let top = thumb_geom(10.0, 400.0, 20.0, 40.0, 0.0, 800.0).unwrap();
        assert!((top.thumb_top - (10.0 + 20.0 + TRACK_PAD)).abs() < 1e-4);
        assert!(top.progress.abs() < 1e-4);

        let bottom = thumb_geom(10.0, 400.0, 20.0, 40.0, 800.0, 800.0).unwrap();
        let track_bottom = 10.0 + 20.0 + bottom.track_height;
        assert!(
            (bottom.thumb_top + bottom.thumb_height - (track_bottom - TRACK_PAD)).abs() < 1e-3,
            "thumb {}+{} vs track bottom {}",
            bottom.thumb_top,
            bottom.thumb_height,
            track_bottom
        );
        assert!((bottom.progress - 1.0).abs() < 1e-4);
    }

    #[test]
    fn thumb_is_proportional_and_floored() {
        let long = thumb_geom(0.0, 400.0, 0.0, 0.0, 0.0, 10_000.0).unwrap();
        assert_eq!(long.thumb_height, MIN_THUMB);

        let short = thumb_geom(0.0, 400.0, 0.0, 0.0, 0.0, 100.0).unwrap();
        let usable = 400.0 - TRACK_PAD * 2.0;
        let expected = usable * (400.0 / 500.0);
        assert!((short.thumb_height - expected).abs() < 1e-3);
        assert!(short.thumb_height > MIN_THUMB);
    }

    #[test]
    fn mouse_mapping_inverts_geometry() {
        let geom = thumb_geom(0.0, 400.0, 0.0, 0.0, 200.0, 800.0).unwrap();
        let grab = geom.thumb_height / 2.0;
        let center = geom.thumb_top + grab;
        let offset = offset_for_mouse(center, grab, geom, 800.0);
        assert!(
            (offset - 200.0).abs() < 1e-2,
            "round-trip offset {offset}, want 200"
        );

        let at_top = offset_for_mouse(geom.track_top, grab, geom, 800.0);
        assert!(at_top < 1.0, "click near top lands at {at_top}");

        let at_bottom = offset_for_mouse(geom.track_top + geom.track_height, grab, geom, 800.0);
        assert!(
            (at_bottom - 800.0).abs() < 1.0,
            "click near bottom lands at {at_bottom}"
        );

        // Pointer left the window: y is unclamped by the platform, but the
        // thumb must stay on the track (same mapping as an out-of-window drag).
        assert!(offset_for_mouse(-200.0, grab, geom, 800.0) < 1.0);
        assert!((offset_for_mouse(2000.0, grab, geom, 800.0) - 800.0).abs() < 1.0);
    }

    #[test]
    fn metrics_key_moves_when_max_offset_grows() {
        use gpui::{Bounds, point, size};

        let viewport = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: size(px(400.0), px(800.0)),
        };
        let before = ScrollMetrics {
            offset: 0.0,
            max_offset: 200.0,
            viewport,
        };
        let after = ScrollMetrics {
            offset: 0.0,
            max_offset: 2000.0,
            viewport,
        };
        assert_ne!(metrics_key(Some(&before)), metrics_key(Some(&after)));
        assert_eq!(metrics_key(Some(&before)), metrics_key(Some(&before)));
        assert_eq!(metrics_key(None), (0, 0, 0));
    }

    #[test]
    fn thumb_colors_follow_appearance() {
        let _guard = lock_appearance();
        set_current_appearance(Appearance::Dark);
        let (dark_rest, dark_hover) = thumb_colors();
        assert!((dark_rest.l - 1.0).abs() < 1e-5, "dark thumb is white ink");
        assert!((dark_hover.l - 1.0).abs() < 1e-5);
        assert!(dark_hover.a > dark_rest.a, "hover is stronger than rest");
        assert!((dark_rest.a - THUMB_REST_ALPHA).abs() < 1e-5);
        assert!((dark_hover.a - THUMB_HOVER_ALPHA).abs() < 1e-5);

        set_current_appearance(Appearance::Light);
        let (light_rest, light_hover) = thumb_colors();
        assert!(light_rest.l < 0.01, "light thumb is black ink");
        assert!(light_hover.l < 0.01);
        assert!(light_hover.a > light_rest.a);
        assert!(
            (light_rest.a - dark_rest.a).abs() < 1e-5,
            "fill alphas are paired across appearances"
        );

        set_current_appearance(Appearance::Dark);
    }
}
