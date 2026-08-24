//! Generic video chrome: a fill-parent player surface and a slim frosted
//! control pill that can be composed into any sized box.
//!
//! Callers own playback. This module paints the surface (click-to-toggle) and
//! the bottom bar: play/pause, a scrub track with elapsed/duration, mute.
//! Parts can be switched off, or assembled independently of the default pill.
//!
//! While playing, the pill fades out after [`CHROME_IDLE`] of no pointer
//! motion (or as soon as the pointer leaves). It fades back in on any move
//! over the player. Paused, loading, and in-progress drags keep it up.

use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, ClickEvent, DispatchPhase, Edges, ElementId, IntoElement, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, RenderOnce,
    SharedString, Window, canvas, div, point, prelude::*, px, quad, size,
};

use crate::frost;
use crate::icons;
use crate::motion;
use crate::theme::{self, Theme};

/// Cap on the control pill. Shrinks with the parent below this.
pub const CONTROLS_MAX_WIDTH: f32 = 600.0;
/// Outer height of the pill, border-box.
pub const PILL_HEIGHT: f32 = 32.0;
/// Inset from the video's bottom and side edges.
pub const CONTROLS_INSET: f32 = 10.0;

const PILL_PAD_X: f32 = 6.0;
const PILL_GAP: f32 = 6.0;
const BUTTON: f32 = 24.0;
const ICON: f32 = 15.0;
const TIME_SIZE: f32 = 9.0;
const TIME_LINE: f32 = 11.0;
const TRACK_THICKNESS: f32 = 3.0;
const TRACK_INSET: f32 = 2.0;
const ICON_REST: f32 = 0.55;
const ICON_HOVER: f32 = 0.95;
const TRACK_WELL_REST: f32 = 0.22;
const TRACK_WELL_HOVER: f32 = 0.38;
/// Settle frames after a seek so a paused player still pulls a new picture.
const SEEK_SETTLE_FRAMES: u8 = 12;
/// Playing chrome hides after this long with no pointer motion over the video.
pub const CHROME_IDLE: Duration = Duration::from_millis(2000);
/// Skip the pill once the fade has landed so a fully hidden bar cannot steal hits.
const CHROME_FADE_VISIBLE: f32 = 0.01;

/// Whether the control pill should be shown.
///
/// Playing chrome is a hover/idle overlay: it stays up while the pointer is
/// over the player and recently moved, while a press/drag is held, or while
/// the file is still loading. Paused chrome stays up so play/seek/mute stay
/// reachable without a hunt.
pub fn chrome_visible(
    playing: bool,
    loading: bool,
    pointer_over: bool,
    held: bool,
    idle: Duration,
) -> bool {
    if !playing || loading || held {
        true
    } else {
        pointer_over && idle < CHROME_IDLE
    }
}

struct ChromeIdle {
    last_move: Instant,
    pointer_over: bool,
    held: bool,
    shown: bool,
    seeded: bool,
    bounds: Option<Bounds<Pixels>>,
}

impl ChromeIdle {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_move: now,
            // Assume the pointer is over the player on first paint (lightbox
            // open is a click on the tile). Idle still hides if they don't move.
            pointer_over: true,
            held: false,
            shown: true,
            seeded: false,
            bounds: None,
        }
    }

    /// True when a redraw is needed (enter/leave, or a move while hidden).
    fn on_move(&mut self, over: bool) -> bool {
        let was_over = self.pointer_over;
        let was_shown = self.shown;
        self.pointer_over = over;
        if over {
            self.last_move = Instant::now();
        }
        over != was_over || (over && !was_shown)
    }
}

/// Playback snapshot the chrome paints. The player lives with the caller.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VideoChrome {
    pub playing: bool,
    pub muted: bool,
    pub loading: bool,
    pub position: f64,
    pub duration: Option<f64>,
}

impl VideoChrome {
    pub fn progress(self) -> f32 {
        progress(self.position, self.duration)
    }

    pub fn elapsed_label(self) -> String {
        format_timecode(Some(self.position))
    }

    pub fn duration_label(self) -> String {
        format_timecode(self.duration)
    }
}

/// Which pieces of the default bottom row to mount.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoParts {
    pub play_pause: bool,
    pub track: bool,
    pub mute: bool,
}

impl VideoParts {
    pub const ALL: Self = Self {
        play_pause: true,
        track: true,
        mute: true,
    };

    pub const NONE: Self = Self {
        play_pause: false,
        track: false,
        mute: false,
    };

    pub fn any(self) -> bool {
        self.play_pause || self.track || self.mute
    }

    pub fn play_pause(mut self, on: bool) -> Self {
        self.play_pause = on;
        self
    }

    pub fn track(mut self, on: bool) -> Self {
        self.track = on;
        self
    }

    pub fn mute(mut self, on: bool) -> Self {
        self.mute = on;
        self
    }
}

impl Default for VideoParts {
    fn default() -> Self {
        Self::ALL
    }
}

/// Clock label for a duration in seconds (`0:06`, `1:05`, `1:00:05`).
pub fn format_timecode(seconds: Option<f64>) -> String {
    let total = seconds.unwrap_or(0.0).max(0.0).round() as u64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let secs = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{secs:02}")
    } else {
        format!("{minutes}:{secs:02}")
    }
}

/// Played fraction in `0..=1`. Missing or zero duration is `0`.
pub fn progress(position: f64, duration: Option<f64>) -> f32 {
    duration
        .filter(|duration| *duration > 0.0)
        .map(|duration| (position / duration).clamp(0.0, 1.0) as f32)
        .unwrap_or(0.0)
}

/// Map a pointer x onto a duration. `None` when the track has no span.
pub fn seek_seconds(x: f32, track_left: f32, track_width: f32, duration: f64) -> Option<f64> {
    if track_width <= f32::EPSILON || duration <= 0.0 {
        return None;
    }
    let t = ((x - track_left) / track_width).clamp(0.0, 1.0);
    Some(t as f64 * duration)
}

type Handler = Rc<dyn Fn(&mut Window, &mut App)>;
type SeekHandler = Rc<dyn Fn(f64, &mut Window, &mut App)>;

/// Fill-parent player: the picture is the caller's children; click toggles
/// play, and the composed control pill sits on the bottom edge.
#[derive(IntoElement)]
pub struct VideoPlayer {
    id: SharedString,
    chrome: VideoChrome,
    parts: VideoParts,
    inset: Edges<f32>,
    children: Vec<AnyElement>,
    on_toggle_play: Option<Handler>,
    on_toggle_mute: Option<Handler>,
    on_seek: Option<SeekHandler>,
}

/// The frosted bottom pill on its own, for callers that already own a surface.
#[derive(IntoElement)]
pub struct VideoControls {
    id: SharedString,
    chrome: VideoChrome,
    parts: VideoParts,
    on_toggle_play: Option<Handler>,
    on_toggle_mute: Option<Handler>,
    on_seek: Option<SeekHandler>,
}

pub fn player(id: impl Into<SharedString>, chrome: VideoChrome) -> VideoPlayer {
    VideoPlayer {
        id: id.into(),
        chrome,
        parts: VideoParts::ALL,
        inset: Edges::default(),
        children: Vec::new(),
        on_toggle_play: None,
        on_toggle_mute: None,
        on_seek: None,
    }
}

pub fn controls(id: impl Into<SharedString>, chrome: VideoChrome) -> VideoControls {
    VideoControls {
        id: id.into(),
        chrome,
        parts: VideoParts::ALL,
        on_toggle_play: None,
        on_toggle_mute: None,
        on_seek: None,
    }
}

impl VideoPlayer {
    pub fn parts(mut self, parts: VideoParts) -> Self {
        self.parts = parts;
        self
    }

    /// Extra inset for the control pill on top of the default
    /// [`CONTROLS_INSET`] placement. `bottom` lifts the pill off the video's
    /// bottom edge (e.g. to clear a sibling overlay like a filmstrip);
    /// `left`/`right` widen the horizontal gutters. `top` is accepted for
    /// symmetry but unused — the pill is bottom-anchored.
    pub fn controls_inset(mut self, inset: Edges<f32>) -> Self {
        self.inset = inset;
        self
    }

    pub fn on_toggle_play(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_toggle_play = Some(Rc::new(f));
        self
    }

    pub fn on_toggle_mute(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_toggle_mute = Some(Rc::new(f));
        self
    }

    pub fn on_seek(mut self, f: impl Fn(f64, &mut Window, &mut App) + 'static) -> Self {
        self.on_seek = Some(Rc::new(f));
        self
    }
}

impl ParentElement for VideoPlayer {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

impl VideoControls {
    pub fn parts(mut self, parts: VideoParts) -> Self {
        self.parts = parts;
        self
    }

    pub fn on_toggle_play(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_toggle_play = Some(Rc::new(f));
        self
    }

    pub fn on_toggle_mute(mut self, f: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_toggle_mute = Some(Rc::new(f));
        self
    }

    pub fn on_seek(mut self, f: impl Fn(f64, &mut Window, &mut App) + 'static) -> Self {
        self.on_seek = Some(Rc::new(f));
        self
    }
}

impl RenderOnce for VideoPlayer {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let VideoPlayer {
            id,
            chrome,
            parts,
            inset,
            children,
            on_toggle_play,
            on_toggle_mute,
            on_seek,
        } = self;
        let controls_id = SharedString::from(format!("{id}-controls"));
        let fade_key = format!("video-chrome:{id}");
        let idle_id: ElementId = SharedString::from(format!("{id}-idle")).into();
        let idle = window.use_keyed_state(idle_id, cx, |_, _| ChromeIdle::new());
        let now = Instant::now();
        let (pointer_over, held, last_move) = {
            let state = idle.read(cx);
            (state.pointer_over, state.held, state.last_move)
        };
        let want = chrome_visible(
            chrome.playing,
            chrome.loading,
            pointer_over,
            held,
            now.saturating_duration_since(last_move),
        );
        idle.update(cx, |state, cx| {
            if chrome.loading || !chrome.playing {
                state.last_move = now;
            }
            if !state.seeded {
                motion::set_hover(&fade_key, true, true);
                state.seeded = true;
                state.shown = true;
            }
            if state.shown != want {
                state.shown = want;
                motion::set_hover(&fade_key, want, motion::reduced_motion(cx));
            }
        });
        let fade = motion::hover_t(&fade_key);
        let mut surface = div()
            .id(id.clone())
            .size_full()
            .relative()
            .overflow_hidden()
            .child({
                let idle_bounds = idle.clone();
                let idle_events = idle.clone();
                canvas(
                    move |bounds, _, cx| {
                        idle_bounds.update(cx, |state, _| {
                            state.bounds = Some(bounds);
                        });
                    },
                    move |_, (), window, _| {
                        window.on_mouse_event({
                            let idle = idle_events.clone();
                            move |event: &MouseMoveEvent, phase, _, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                let Some(bounds) = idle.read(cx).bounds else {
                                    return;
                                };
                                let over = bounds.contains(&event.position);
                                idle.update(cx, |state, cx| {
                                    if state.on_move(over) {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                        window.on_mouse_event({
                            let idle = idle_events.clone();
                            move |event: &MouseDownEvent, phase, _, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                let Some(bounds) = idle.read(cx).bounds else {
                                    return;
                                };
                                if !bounds.contains(&event.position) {
                                    return;
                                }
                                idle.update(cx, |state, cx| {
                                    state.held = true;
                                    state.last_move = Instant::now();
                                    state.pointer_over = true;
                                    if !state.shown {
                                        cx.notify();
                                    }
                                });
                            }
                        });
                        window.on_mouse_event({
                            let idle = idle_events;
                            move |_: &MouseUpEvent, phase, _, cx| {
                                if phase != DispatchPhase::Bubble {
                                    return;
                                }
                                idle.update(cx, |state, cx| {
                                    if !state.held {
                                        return;
                                    }
                                    state.held = false;
                                    state.last_move = Instant::now();
                                    cx.notify();
                                });
                            }
                        });
                    },
                )
                .absolute()
                .inset_0()
                .size_full()
            })
            .children(children);
        // Sibling on top of the frame, under the pill: native surfaces can eat
        // hits, so a parent `on_click` is not enough.
        if let Some(handler) = on_toggle_play.clone() {
            surface = surface.child(
                div()
                    .id(SharedString::from(format!("{id}-hit")))
                    .absolute()
                    .inset_0()
                    .cursor_pointer()
                    .on_click(
                        move |event: &ClickEvent, window: &mut Window, cx: &mut App| {
                            if event.click_count() == 1 {
                                handler(window, cx);
                            }
                            cx.stop_propagation();
                        },
                    ),
            );
        }
        let show_chrome = parts.any() && (want || fade > CHROME_FADE_VISIBLE);
        surface.when(show_chrome, move |surface| {
            let mut bar = controls(controls_id, chrome).parts(parts);
            if let Some(handler) = on_toggle_play {
                bar = bar.on_toggle_play(move |window, cx| handler(window, cx));
            }
            if let Some(handler) = on_toggle_mute {
                bar = bar.on_toggle_mute(move |window, cx| handler(window, cx));
            }
            if let Some(handler) = on_seek {
                bar = bar.on_seek(move |seconds, window, cx| handler(seconds, window, cx));
            }
            surface.child(frost::layered(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .bottom(px(CONTROLS_INSET + inset.bottom))
                    .pl(px(CONTROLS_INSET + inset.left))
                    .pr(px(CONTROLS_INSET + inset.right))
                    .flex()
                    .justify_center()
                    .opacity(fade)
                    .child(bar),
            ))
        })
    }
}

impl RenderOnce for VideoControls {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let VideoControls {
            id,
            chrome,
            parts,
            on_toggle_play,
            on_toggle_mute,
            on_seek,
        } = self;
        if !parts.any() {
            return div().into_any_element();
        }
        let theme = Theme::of(cx);
        let fill = if theme.is_frost() {
            theme.wash(0.3)
        } else {
            theme.surface_overlay
        };
        let pill_id = id.clone();
        let mut pill = div()
            .id(id)
            .h(px(PILL_HEIGHT))
            .w_full()
            .px(px(PILL_PAD_X))
            .flex()
            .flex_row()
            .items_center()
            .gap(px(PILL_GAP))
            .rounded(px(PILL_HEIGHT / 2.0))
            .border_1()
            .border_color(theme::hairline(0.10))
            .bg(fill)
            .overflow_hidden()
            .occlude()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_mouse_up(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .on_click(|_, _, cx| cx.stop_propagation());
        if parts.play_pause {
            let play_id = SharedString::from(format!("{pill_id}-play"));
            pill = pill.child(play_pause_button(
                play_id,
                chrome.playing,
                chrome.loading,
                on_toggle_play
                    .clone()
                    .map(|handler| move |window: &mut Window, cx: &mut App| handler(window, cx)),
            ));
        }
        if parts.track {
            let track_id = SharedString::from(format!("{pill_id}-track"));
            pill = pill.child(
                scrub_track(
                    track_id,
                    chrome,
                    theme.font_mono.clone(),
                    on_seek,
                    window,
                    cx,
                )
                .flex_1()
                .min_w(px(0.0)),
            );
        }
        if parts.mute {
            let mute_id = SharedString::from(format!("{pill_id}-mute"));
            pill = pill.child(mute_button(
                mute_id,
                chrome.muted,
                on_toggle_mute
                    .map(|handler| move |window: &mut Window, cx: &mut App| handler(window, cx)),
            ));
        }
        div()
            .w_full()
            .max_w(px(CONTROLS_MAX_WIDTH))
            .flex_none()
            .child(frost::frosted(PILL_HEIGHT / 2.0, frost::MENU_BLUR, pill))
            .into_any_element()
    }
}

/// Play/pause glyph button. Compose into a custom bar, or let [`controls`]
/// place it on the left of the default pill.
pub fn play_pause_button(
    id: impl Into<SharedString>,
    playing: bool,
    loading: bool,
    on_click: Option<impl Fn(&mut Window, &mut App) + 'static>,
) -> gpui::Stateful<gpui::Div> {
    let icon = if playing { icons::PAUSE } else { icons::PLAY };
    let rest = if loading { ICON_REST * 0.6 } else { ICON_REST };
    let hover = if loading { ICON_REST } else { ICON_HOVER };
    let mut button = icon_button(id, icon, rest, hover);
    if let Some(handler) = on_click {
        button = button
            .cursor_pointer()
            .on_click(move |_, window: &mut Window, cx: &mut App| {
                handler(window, cx);
                cx.stop_propagation();
            });
    }
    button
}

/// Mute/unmute glyph button. Compose into a custom bar, or let [`controls`]
/// place it on the right of the default pill.
pub fn mute_button(
    id: impl Into<SharedString>,
    muted: bool,
    on_click: Option<impl Fn(&mut Window, &mut App) + 'static>,
) -> gpui::Stateful<gpui::Div> {
    let icon = if muted {
        icons::VOLUME_CROSS
    } else {
        icons::VOLUME_LOUD
    };
    let mut button = icon_button(id, icon, ICON_REST, ICON_HOVER);
    if let Some(handler) = on_click {
        button = button
            .cursor_pointer()
            .on_click(move |_, window: &mut Window, cx: &mut App| {
                handler(window, cx);
                cx.stop_propagation();
            });
    }
    button
}

fn icon_button(
    id: impl Into<SharedString>,
    icon: &'static str,
    rest: f32,
    hover: f32,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let hover_key = format!("video-btn:{id}");
    div()
        .id(id)
        .size(px(BUTTON))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .on_hover(motion::hover_listener(hover_key.clone()))
        .child(
            icons::icon(icon)
                .size(px(ICON))
                .text_color(motion::hover_blend(
                    &hover_key,
                    theme::ink(rest),
                    theme::ink(hover),
                )),
        )
}

fn scrub_track(
    id: SharedString,
    chrome: VideoChrome,
    font_mono: SharedString,
    on_seek: Option<SeekHandler>,
    window: &mut Window,
    cx: &mut App,
) -> gpui::Stateful<gpui::Div> {
    let element_id: ElementId = id.clone().into();
    let hover_key = format!("video-track:{id}");
    let state = window.use_keyed_state(element_id.clone(), cx, |_, _| TrackState::default());
    let dragging = state.read(cx).drag;
    let hover_t = if dragging {
        1.0
    } else {
        motion::hover_t(&hover_key)
    };
    let t = chrome.progress();
    let duration = chrome.duration.unwrap_or(0.0);
    let on_seek_down = on_seek.clone();
    let state_down = state.clone();
    let mut track = div()
        .id(id)
        .relative()
        .h_full()
        .min_w(px(0.0))
        .overflow_hidden()
        .cursor_pointer()
        .on_hover(motion::hover_listener(hover_key.clone()))
        .on_mouse_down(MouseButton::Left, move |event, window, cx| {
            let bounds = state_down.read(cx).bounds;
            let Some(bounds) = bounds else {
                return;
            };
            let (left, width) = track_span(bounds);
            if let Some(seconds) = seek_seconds(f32::from(event.position.x), left, width, duration)
            {
                if let Some(handler) = on_seek_down.as_ref() {
                    handler(seconds, window, cx);
                }
                state_down.update(cx, |state, cx| {
                    state.drag = true;
                    cx.notify();
                });
            }
            cx.stop_propagation();
        });
    track = track.child({
        let state_paint = state.clone();
        let on_seek_move = on_seek.clone();
        canvas(
            |_, _, _| {},
            move |bounds, (), window, cx| {
                state_paint.update(cx, |state, _| {
                    state.bounds = Some(bounds);
                });
                paint_track(bounds, t, hover_t, window);
                let state_move = state_paint.clone();
                let on_seek = on_seek_move.clone();
                window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble || !event.dragging() {
                        return;
                    }
                    if !state_move.read(cx).drag {
                        return;
                    }
                    let Some(bounds) = state_move.read(cx).bounds else {
                        return;
                    };
                    let (left, width) = track_span(bounds);
                    if let Some(seconds) =
                        seek_seconds(f32::from(event.position.x), left, width, duration)
                        && let Some(handler) = on_seek.as_ref()
                    {
                        handler(seconds, window, cx);
                    }
                });
                let state_up = state_paint.clone();
                window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                    if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                        return;
                    }
                    if !state_up.read(cx).drag {
                        return;
                    }
                    state_up.update(cx, |state, cx| {
                        state.drag = false;
                        cx.notify();
                    });
                });
            },
        )
        .absolute()
        .inset_0()
        .size_full()
    });
    let time_color = motion::mix(theme::ink(ICON_REST), theme::ink(ICON_HOVER), hover_t);
    track
        .child(timecode_label(
            chrome.elapsed_label(),
            font_mono.clone(),
            time_color,
            true,
        ))
        .child(timecode_label(
            chrome.duration_label(),
            font_mono,
            time_color,
            false,
        ))
}

fn timecode_label(
    label: String,
    font_mono: SharedString,
    color: gpui::Hsla,
    left: bool,
) -> gpui::Div {
    let mut chip = div()
        .absolute()
        .top(px(2.0))
        .text_size(px(TIME_SIZE))
        .line_height(px(TIME_LINE))
        .font_family(font_mono)
        .font_weight(gpui::FontWeight::MEDIUM)
        .text_color(color)
        .child(SharedString::from(label));
    if left {
        chip = chip.left(px(TRACK_INSET));
    } else {
        chip = chip.right(px(TRACK_INSET));
    }
    chip
}

#[derive(Default)]
struct TrackState {
    bounds: Option<Bounds<Pixels>>,
    drag: bool,
}

fn track_span(bounds: Bounds<Pixels>) -> (f32, f32) {
    let left = f32::from(bounds.origin.x) + TRACK_INSET;
    let width = (f32::from(bounds.size.width) - TRACK_INSET * 2.0).max(0.0);
    (left, width)
}

fn paint_track(bounds: Bounds<Pixels>, t: f32, hover_t: f32, window: &mut Window) {
    let (left, width) = track_span(bounds);
    if width <= 0.0 {
        return;
    }
    // Sit the bar under the timecodes so 9px type rests on top of the track.
    let mid_y = bounds.origin.y + bounds.size.height * 0.62;
    let origin_x = px(left);
    let fill = width * t.clamp(0.0, 1.0);
    let well = motion::mix(
        theme::ink(TRACK_WELL_REST),
        theme::ink(TRACK_WELL_HOVER),
        hover_t,
    );
    let played = motion::mix(theme::ink(ICON_REST), theme::ink(ICON_HOVER), hover_t);
    window.paint_quad(quad(
        Bounds {
            origin: point(origin_x, mid_y - px(TRACK_THICKNESS / 2.0)),
            size: size(px(width), px(TRACK_THICKNESS)),
        },
        px(TRACK_THICKNESS / 2.0),
        well,
        px(0.0),
        gpui::transparent_black(),
        gpui::BorderStyle::default(),
    ));
    if fill > 0.0 {
        window.paint_quad(quad(
            Bounds {
                origin: point(origin_x, mid_y - px(TRACK_THICKNESS / 2.0)),
                size: size(px(fill), px(TRACK_THICKNESS)),
            },
            px(TRACK_THICKNESS / 2.0),
            played,
            px(0.0),
            gpui::transparent_black(),
            gpui::BorderStyle::default(),
        ));
    }
}

/// Frames the in-app player keeps pulling after a seek while paused.
pub fn seek_settle_frames() -> u8 {
    SEEK_SETTLE_FRAMES
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timecode_is_clock_style() {
        assert_eq!(format_timecode(Some(0.0)), "0:00");
        assert_eq!(format_timecode(Some(6.0)), "0:06");
        assert_eq!(format_timecode(Some(6.2)), "0:06");
        assert_eq!(format_timecode(Some(65.4)), "1:05");
        assert_eq!(format_timecode(Some(75.0)), "1:15");
        assert_eq!(format_timecode(Some(3605.0)), "1:00:05");
        assert_eq!(format_timecode(None), "0:00");
    }

    #[test]
    fn progress_clamps_and_defaults() {
        assert_eq!(progress(0.0, Some(10.0)), 0.0);
        assert_eq!(progress(5.0, Some(10.0)), 0.5);
        assert_eq!(progress(10.0, Some(10.0)), 1.0);
        assert_eq!(progress(12.0, Some(10.0)), 1.0);
        assert_eq!(progress(5.0, Some(0.0)), 0.0);
        assert_eq!(progress(5.0, None), 0.0);
        assert_eq!(progress(-1.0, Some(10.0)), 0.0);
    }

    #[test]
    fn seek_seconds_maps_the_track() {
        assert_eq!(seek_seconds(50.0, 0.0, 100.0, 10.0), Some(5.0));
        assert_eq!(seek_seconds(0.0, 0.0, 100.0, 10.0), Some(0.0));
        assert_eq!(seek_seconds(100.0, 0.0, 100.0, 10.0), Some(10.0));
        assert_eq!(seek_seconds(-10.0, 0.0, 100.0, 10.0), Some(0.0));
        assert_eq!(seek_seconds(200.0, 0.0, 100.0, 10.0), Some(10.0));
        assert_eq!(seek_seconds(50.0, 0.0, 100.0, 0.0), None);
        assert_eq!(seek_seconds(50.0, 0.0, 0.0, 10.0), None);
        assert_eq!(seek_seconds(70.0, 20.0, 100.0, 8.0), Some(4.0));
    }

    #[test]
    fn chrome_labels_follow_position_and_duration() {
        let chrome = VideoChrome {
            position: 6.2,
            duration: Some(75.0),
            ..VideoChrome::default()
        };
        assert_eq!(chrome.elapsed_label(), "0:06");
        assert_eq!(chrome.duration_label(), "1:15");
        assert!((chrome.progress() - 6.2 / 75.0).abs() < 1e-5);
    }

    #[test]
    fn parts_can_drop_any_slot() {
        let parts = VideoParts::ALL.mute(false);
        assert!(parts.play_pause && parts.track && !parts.mute);
        assert!(parts.any());
        assert!(!VideoParts::NONE.any());
        assert_eq!(VideoParts::default(), VideoParts::ALL);
    }

    #[test]
    fn chrome_stays_up_when_paused_or_loading() {
        assert!(chrome_visible(
            false,
            false,
            false,
            false,
            Duration::from_secs(30)
        ));
        assert!(chrome_visible(
            true,
            true,
            false,
            false,
            Duration::from_secs(30)
        ));
        assert!(chrome_visible(
            true,
            false,
            false,
            true,
            Duration::from_secs(30)
        ));
    }

    #[test]
    fn playing_chrome_follows_pointer_and_idle() {
        assert!(chrome_visible(
            true,
            false,
            true,
            false,
            Duration::from_millis(0)
        ));
        assert!(chrome_visible(
            true,
            false,
            true,
            false,
            CHROME_IDLE - Duration::from_millis(1)
        ));
        assert!(!chrome_visible(true, false, true, false, CHROME_IDLE));
        assert!(!chrome_visible(
            true,
            false,
            false,
            false,
            Duration::from_millis(0)
        ));
    }
}
