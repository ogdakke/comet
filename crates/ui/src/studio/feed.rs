//! Conversation feed: virtualized turns, tiles, and the tick rail.
//!
//! One [`gpui::ListState`] item per turn — the same variable-height list the
//! agent transcript uses — so long conversations keep measured heights and only
//! paint the visible window plus overdraw. Image fetches follow the gallery:
//! visible turns load full frames, neighbors prefetch thumbs.

use std::ops::Range;
use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use gpui::{
    AnyElement, Context, ListAlignment, ListOffset, ListScrollEvent, ListState, SharedString,
    Window, canvas, div, list, prelude::*, px,
};
use zeron_proto::{StudioRunState, StudioRunView, StudioTurnView};
use zeron_studio::{MediaKind, StudioArtifactId};

use crate::icons;
use crate::motion;
use crate::popover;
use crate::rail;
use crate::shader::{Effect, shader};
use crate::state::format_time_ago;
use crate::theme::Theme;
use crate::transcript::{self, format_timestamp};

use super::artifact::contain_image;
use super::page::StudioPage;

/// Scroll runway below the final Studio turn. The composer floats 18px above
/// the viewport and is 191px tall at its largest first-release configuration;
/// a 30px generate-more pill sits 10px above that. The remaining space keeps
/// the last image clear of the glass card.
pub(super) const STUDIO_COMPOSER_CLEARANCE: f32 = 296.0;
/// Extra left inset so the tick rail (16px + 20px hover bar) does not cover
/// the prompt header. Matches the chat transcript's wide-gutter band.
pub(super) const STUDIO_RAIL_GUTTER: f32 = 28.0;
/// Bottom of the unobstructed reading band. The composer floats 18px above
/// the viewport and is ~191px tall; ticks should ignore that covered strip
/// when deciding which turn you are looking at.
const STUDIO_READING_BOTTOM_INSET: f32 = 220.0;
/// Space between successive turns. Previously the flex gap of the
/// unvirtualized column; now the non-last row's bottom pad.
const FEED_TURN_GAP: f32 = 28.0;
/// Paint / measure window past the viewport — same order as the gallery so a
/// fling reveals the next turn already decoded.
const FEED_OVERDRAW_PX: f32 = 1600.0;
/// Turns above and below the visible range that prefetch thumbs.
const FEED_PREFETCH_TURNS: usize = 2;
/// Horizontal inset matching the previous overflow-scroll padding.
pub(super) const FEED_PAD_X: f32 = 24.0;
/// Collapsed prompt bubble: three rows of chat-bubble type, then Show more.
const PROMPT_COLLAPSED_LINES: usize = 3;
const PROMPT_LINE_HEIGHT: f32 = 22.0;
const PROMPT_BUBBLE_PAD_X: f32 = 16.0;
/// Approximate Geist 14px Latin advance — wrap estimates without a layout pass.
pub(super) const PROMPT_AVG_CHAR_ADVANCE: f32 = 7.4;

/// One feed-rail tick: a Studio turn's prompt, the models that ran it, and
/// when it was sent.
#[derive(Debug, Clone, PartialEq)]
struct StudioRailTick {
    turn_ix: usize,
    prompt: String,
    models: Vec<(String, u32)>,
    created_at: DateTime<Utc>,
    cost: Option<String>,
}

/// Measured turn interval in the same coordinate space as the list viewport.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TurnSpan {
    top: f32,
    bottom: f32,
}

impl TurnSpan {
    fn overlap(self, band_top: f32, band_bottom: f32) -> f32 {
        (self.bottom.min(band_bottom) - self.top.max(band_top)).max(0.0)
    }
}

/// The most plausible Studio rail tick for the current scroll position.
///
/// Studio turns are tall — image grids plus a 296px last-row composer pad —
/// so the viewport-top row is often still the previous turn while you are
/// looking at the next one, and even when you are pinned to the end.
///
/// 1. Empty → `None`.
/// 2. Pinned to the end → last tick.
/// 3. Otherwise the turn with the largest intersection with the reading
///    band (viewport minus the floating composer). Exact ties go to the
///    later turn, so a 50/50 split prefers what you just scrolled into.
/// 4. If nothing intersects the band, the last turn whose top is at or
///    above the band top; before any turn reaches it, the first.
fn studio_active_tick(
    spans: &[TurnSpan],
    reading_top: f32,
    reading_bottom: f32,
    at_end: bool,
) -> Option<usize> {
    if spans.is_empty() {
        return None;
    }
    if at_end {
        return Some(spans.len() - 1);
    }
    let band_bottom = reading_bottom.max(reading_top);
    let mut best: Option<(usize, f32)> = None;
    for (ix, span) in spans.iter().enumerate() {
        let overlap = span.overlap(reading_top, band_bottom);
        match best {
            Some((_, best_overlap)) if overlap < best_overlap => {}
            _ if overlap > 0.0 => best = Some((ix, overlap)),
            _ => {}
        }
    }
    if let Some((ix, _)) = best {
        return Some(ix);
    }
    match spans.iter().rposition(|span| span.top <= reading_top) {
        Some(ix) => Some(ix),
        None => Some(0),
    }
}

fn studio_rail_ticks(turns: &[StudioTurnView]) -> Vec<StudioRailTick> {
    turns
        .iter()
        .enumerate()
        .map(|(turn_ix, turn)| StudioRailTick {
            turn_ix,
            prompt: turn.prompt.clone(),
            models: merge_studio_models(&turn.runs),
            created_at: turn.created_at,
            cost: super::cost::turn_quote(turn)
                .as_ref()
                .map(super::cost::format_quote),
        })
        .collect()
}

/// Collapse repeated generate-more copies of the same model into one rail line.
fn merge_studio_models(runs: &[StudioRunView]) -> Vec<(String, u32)> {
    let mut models: Vec<(String, u32)> = Vec::new();
    for run in runs {
        if let Some((_, count)) = models
            .iter_mut()
            .find(|(name, _)| name == &run.model.display_name)
        {
            *count = count.saturating_add(run.output_count);
        } else {
            models.push((run.model.display_name.clone(), run.output_count));
        }
    }
    models
}

/// Compact "model · n" list for the hover card. One model spells out
/// "variation(s)"; several stay short so the card stays one-scan.
fn format_studio_models(models: &[(String, u32)]) -> String {
    match models {
        [] => String::new(),
        [(name, 1)] => format!("{name} · 1 variation"),
        [(name, count)] => format!("{name} · {count} variations"),
        many => many
            .iter()
            .map(|(name, count)| format!("{name} · {count}"))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Sidebar-style relative time ("5m", "3h") plus the transcript's absolute
/// send clock ("Jul 1, 3:45 PM"). Recent rows read like the chat list;
/// older ones still name the moment they were sent.
fn format_studio_tick_time<Tz: TimeZone>(then: DateTime<Utc>, now: DateTime<Utc>, tz: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let relative = format_time_ago(then, now);
    let absolute = format_timestamp(then.timestamp_millis(), tz);
    if absolute.is_empty() {
        relative
    } else {
        format!("{relative} · {absolute}")
    }
}

/// Images that still exist on the conversation — requested-but-missing
/// slots and non-image artifacts do not count.
pub(super) fn conversation_image_count(turns: &[StudioTurnView]) -> u32 {
    turns
        .iter()
        .flat_map(|turn| &turn.runs)
        .flat_map(|run| &run.artifacts)
        .filter(|artifact| artifact.media_kind == MediaKind::Image)
        .count() as u32
}

pub(super) fn turn_index_for_artifact(
    turns: &[StudioTurnView],
    artifact_id: StudioArtifactId,
) -> Option<usize> {
    turns.iter().position(|turn| {
        turn.runs.iter().any(|run| {
            run.artifacts
                .iter()
                .any(|artifact| artifact.id == artifact_id)
        })
    })
}

/// Hold the Open-in-thread ring at full strength, then fade it out.
pub(super) const ARTIFACT_FOCUS_HOLD: Duration = Duration::from_millis(2600);
pub(super) const ARTIFACT_FOCUS_FADE: Duration = Duration::from_millis(400);

pub(super) fn artifact_focus_alpha(elapsed: Duration) -> Option<f32> {
    let total = ARTIFACT_FOCUS_HOLD + ARTIFACT_FOCUS_FADE;
    if elapsed >= total {
        None
    } else if elapsed <= ARTIFACT_FOCUS_HOLD {
        Some(1.0)
    } else {
        let t = (elapsed - ARTIFACT_FOCUS_HOLD).as_secs_f32()
            / ARTIFACT_FOCUS_FADE.as_secs_f32().max(0.001);
        Some((1.0 - t).clamp(0.0, 1.0))
    }
}

pub(super) struct ArtifactFeedTarget {
    pub turn_ix: usize,
    pub offset: f32,
}

/// Turn row and y-offset of `artifact_id` inside that turn, so the feed can
/// land on the tile instead of the prompt.
pub(super) fn artifact_feed_target(
    turns: &[StudioTurnView],
    artifact_id: StudioArtifactId,
    content_width: f32,
    tile_width: f32,
    gap: f32,
    columns: usize,
) -> Option<ArtifactFeedTarget> {
    let turn_ix = turn_index_for_artifact(turns, artifact_id)?;
    let turn = &turns[turn_ix];
    let columns = columns.max(1);
    let header = estimated_turn_header(turn, content_width);
    let mut slot = 0usize;
    for run in &turn.runs {
        for (_, id) in feed_output_slots(run) {
            if id == Some(artifact_id) {
                let (aw, ah) = run.display_aspect_ratio;
                let tile_h = tile_width * ah as f32 / aw.max(1) as f32;
                let row = slot / columns;
                return Some(ArtifactFeedTarget {
                    turn_ix,
                    offset: header + row as f32 * (tile_h + gap),
                });
            }
            slot += 1;
        }
    }
    Some(ArtifactFeedTarget {
        turn_ix,
        offset: header,
    })
}

fn estimated_turn_header(turn: &StudioTurnView, content_width: f32) -> f32 {
    let bubble_width = content_width
        .min(1600.0)
        .min(crate::transcript::MAX_CONTENT_WIDTH * 0.8);
    let inner = (bubble_width - PROMPT_BUBBLE_PAD_X * 2.0).max(1.0);
    let lines = prompt_visual_lines_at(&turn.prompt, inner, PROMPT_AVG_CHAR_ADVANCE)
        .clamp(1, PROMPT_COLLAPSED_LINES);
    let prompt_h = 20.0 + lines as f32 * PROMPT_LINE_HEIGHT;
    prompt_h + 8.0 + 24.0 + 12.0
}

/// Height-affecting identity of the feed. Remeasure only when this changes
/// so progress ticks do not reset the virtualizer.
#[derive(Clone, PartialEq)]
pub(super) struct FeedLayoutSig {
    tile_q: u32,
    columns: usize,
    turns: Vec<TurnLayoutSig>,
}

#[derive(Clone, PartialEq)]
struct TurnLayoutSig {
    id: zeron_studio::StudioTurnId,
    expanded: bool,
    slots: Vec<(u16, u16, u16)>,
}

pub(super) fn new_feed_list(cx: &mut Context<StudioPage>) -> ListState {
    let list = ListState::new(0, ListAlignment::Top, px(FEED_OVERDRAW_PX)).measure_all();
    let weak = cx.weak_entity();
    list.set_scroll_handler(move |event: &ListScrollEvent, _, cx| {
        weak.update(cx, |page: &mut StudioPage, cx| {
            page.feed_visible_rows = event.visible_range.clone();
            page.request_visible_feed_images(cx);
            cx.notify();
        })
        .ok();
    });
    list
}

fn remeasure_changed_feed_rows(list: &ListState, old: Option<&FeedLayoutSig>, new: &FeedLayoutSig) {
    let Some(old) = old else {
        list.remeasure_items(0..new.turns.len());
        return;
    };
    let shared = old.turns.len().min(new.turns.len());
    if old.columns != new.columns || old.tile_q != new.tile_q {
        list.remeasure_items(0..shared);
        return;
    }
    for i in 0..shared {
        if old.turns[i] != new.turns[i] {
            list.remeasure_items(i..i + 1);
        }
    }
}

/// Image ids on `turns[range]`. Non-image artifacts and holes are skipped.
pub(super) fn feed_image_ids(
    turns: &[StudioTurnView],
    range: Range<usize>,
) -> Vec<StudioArtifactId> {
    let end = range.end.min(turns.len());
    let start = range.start.min(end);
    turns[start..end]
        .iter()
        .flat_map(|turn| &turn.runs)
        .flat_map(|run| &run.artifacts)
        .filter(|artifact| artifact.media_kind == MediaKind::Image)
        .map(|artifact| artifact.id)
        .collect()
}

/// Visible range if the list has reported one; otherwise the tail — opening a
/// conversation lands on the latest turn, so prefetching from 0 would decode
/// the wrong images on the first frame.
pub(super) fn feed_visible_or_tail(visible: Range<usize>, turn_count: usize) -> Range<usize> {
    if visible.end > visible.start {
        visible.start.min(turn_count)..visible.end.min(turn_count)
    } else {
        turn_count.saturating_sub(3)..turn_count
    }
}

/// Visible `[start, end)` from the list's current top item. `top_item >=
/// item_count` is `scroll_to_end`'s past-the-last anchor — treat it as the
/// tail. `span` is how many measured items still intersect the viewport.
pub(super) fn feed_visible_from_top(
    top_item: usize,
    item_count: usize,
    span: usize,
) -> Range<usize> {
    if item_count == 0 {
        return 0..0;
    }
    if top_item >= item_count {
        return item_count.saturating_sub(span.max(1))..item_count;
    }
    top_item..(top_item + span.max(1)).min(item_count)
}

pub fn grid_columns(content_width: f32) -> usize {
    if content_width < 520.0 {
        1
    } else if content_width < 900.0 {
        2
    } else if content_width < 1240.0 {
        3
    } else {
        4
    }
}

/// Slack around the 520 / 900 / 1240 cuts so a 1px measure wobble cannot
/// flip 2↔3↔4 columns while the window edge is being dragged.
const COLUMN_SLACK: f32 = 32.0;
/// Remeasure the virtualizer only when tile width moves by this much.
const TILE_QUANT: f32 = 8.0;

fn column_enter_width(columns: usize) -> f32 {
    match columns {
        2 => 520.0,
        3 => 900.0,
        4 => 1240.0,
        _ => 0.0,
    }
}

pub(super) fn grid_columns_sticky(content_width: f32, current: usize) -> usize {
    let desired = grid_columns(content_width);
    if current == 0 || desired == current {
        return desired;
    }
    if desired > current {
        if content_width >= column_enter_width(desired) + COLUMN_SLACK {
            desired
        } else {
            current
        }
    } else if content_width + COLUMN_SLACK < column_enter_width(current) {
        desired
    } else {
        current
    }
}

/// Visual rows a prompt would occupy at `inner_width` (bubble minus padding).
/// Newlines always take a row, including blank ones.
#[cfg(test)]
fn prompt_visual_lines(prompt: &str, inner_width: f32) -> usize {
    prompt_visual_lines_at(prompt, inner_width, PROMPT_AVG_CHAR_ADVANCE)
}

/// Same wrap estimate as [`prompt_visual_lines`], with an explicit advance so
/// the 12px inspector prompt can share the math.
pub(super) fn prompt_visual_lines_at(prompt: &str, inner_width: f32, char_advance: f32) -> usize {
    let cols = (inner_width / char_advance.max(0.1)).floor().max(1.0) as usize;
    prompt
        .split('\n')
        .map(|line| {
            let chars = line.chars().count();
            if chars == 0 { 1 } else { chars.div_ceil(cols) }
        })
        .sum()
}

fn prompt_exceeds_collapsed_lines(prompt: &str, inner_width: f32) -> bool {
    prompt_exceeds_lines(
        prompt,
        inner_width,
        PROMPT_AVG_CHAR_ADVANCE,
        PROMPT_COLLAPSED_LINES,
    )
}

pub(super) fn prompt_exceeds_lines(
    prompt: &str,
    inner_width: f32,
    char_advance: f32,
    max_lines: usize,
) -> bool {
    prompt_visual_lines_at(prompt, inner_width, char_advance) > max_lines
}

/// Quiet text action under a prompt bubble or image grid (`Use prompt`,
/// earlier-turn `Generate more`). The latest turn's generate-more lives on
/// the floating composer pill instead.
pub(super) fn turn_action(
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let id = id.into();
    let fade = id.to_string();
    div()
        .id(id)
        .flex_none()
        .flex()
        .items_center()
        .gap(px(4.0))
        .cursor_pointer()
        .text_size(px(11.0))
        .line_height(px(16.0))
        .text_color(motion::hover_blend(
            &fade,
            theme.text_muted.opacity(0.7),
            theme.text,
        ))
        .on_hover(motion::hover_listener(SharedString::from(fade)))
        .child(label.into())
}

/// Inline clamp toggle used by the feed bubble and the artifact inspector.
pub(super) fn show_more_action(
    id: impl Into<SharedString>,
    expanded: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    let more = !expanded;
    let fade = id.into();
    let chevron = if more {
        icons::ALT_ARROW_DOWN
    } else {
        icons::ARROW_UP
    };
    turn_action(
        fade.clone(),
        if more { "Show more" } else { "Show less" },
        theme,
    )
    .child(
        icons::icon(chevron)
            .size(px(11.0))
            .text_color(motion::hover_blend(
                &fade,
                theme.text_muted.opacity(0.7),
                theme.text,
            )),
    )
}

/// Tiles to paint for a run. In-flight and failed runs keep a slot for every
/// requested output; a succeeded run only shows artifacts that still exist so
/// a delete cannot leave a "Loading image" hole.
fn feed_output_slots(run: &StudioRunView) -> Vec<(usize, Option<StudioArtifactId>)> {
    if run.state == StudioRunState::Succeeded {
        run.artifacts
            .iter()
            .map(|artifact| (artifact.output_position as usize, Some(artifact.id)))
            .collect()
    } else {
        (0..run.output_count as usize)
            .map(|output_ix| {
                let artifact = run
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.output_position as usize == output_ix)
                    .map(|artifact| artifact.id);
                (output_ix, artifact)
            })
            .collect()
    }
}

fn lattice_seed(turn_ix: usize, run_ix: usize, output_ix: usize) -> u32 {
    let mut x = (turn_ix as u32).wrapping_mul(0x9E37_79B9)
        ^ (run_ix as u32).wrapping_mul(0x85EB_CA6B)
        ^ (output_ix as u32).wrapping_mul(0xC2B2_AE35);
    x ^= x >> 16;
    x = x.wrapping_mul(0x7FEB_352D);
    x ^= x >> 15;
    x
}

impl StudioPage {
    pub(super) fn render_tile(
        &self,
        turn_ix: usize,
        run_ix: usize,
        output_ix: usize,
        width: f32,
        aspect: (u32, u32),
        state: StudioRunState,
        artifact_id: Option<StudioArtifactId>,
        progress: Option<f32>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let height = width * aspect.1 as f32 / aspect.0.max(1) as f32;
        let mut base = div()
            .id(SharedString::from(format!(
                "studio-tile-{turn_ix}-{run_ix}-{output_ix}"
            )))
            .w(px(width))
            .h(px(height))
            .flex_none()
            .rounded(px(10.0))
            .overflow_hidden()
            .bg(crate::theme::ink(if state == StudioRunState::Failed {
                0.08
            } else {
                0.045
            }));
        if let Some(id) = artifact_id
            && let Some(conversation_id) = self.selected_conversation
        {
            base = self.bind_image_menu(
                base,
                id,
                conversation_id,
                super::image_menu::ImageSurface::ThreadTile,
                cx,
            );
        }
        if let Some(id) = artifact_id {
            let (image, full) = self.display_layers(id, window, cx);
            if let Some(image) = image {
                return base
                    .relative()
                    .cursor_pointer()
                    .on_hover(cx.listener(move |page, hovered: &bool, window, cx| {
                        if *hovered {
                            page.prefetch_gallery_full(id, window, cx);
                        }
                    }))
                    .on_click(cx.listener(move |page, _, window, cx| {
                        let frames = page
                            .conversation
                            .as_ref()
                            .map(super::artifact::frames_from_conversation)
                            .unwrap_or_default();
                        page.open_artifact_viewer(id, frames, cx);
                        window.focus(&page.focus, cx);
                    }))
                    .child(crate::motion::fade_quick(
                        SharedString::from(format!("studio-image-reveal-{}", id.0)),
                        div()
                            .size_full()
                            .relative()
                            .child(contain_image(image).size_full().rounded(px(10.0)))
                            .when_some(full, |layer, full| {
                                layer.child(
                                    contain_image(full).absolute().inset_0().rounded(px(10.0)),
                                )
                            }),
                    ))
                    .when_some(self.artifact_focus_ring(id, theme), |tile, ring| {
                        tile.child(ring)
                    })
                    .into_any_element();
            }
        }
        let pending = matches!(
            state,
            StudioRunState::Queued | StudioRunState::Running | StudioRunState::Downloading
        );
        let seed = lattice_seed(turn_ix, run_ix, output_ix);
        let fill = pending.then(|| {
            let effect = match state {
                StudioRunState::Queued => Effect::SoftNoise { seed, amount: 0.7 },
                StudioRunState::Downloading => Effect::StarShimmer { seed, speed: 1.0 },
                _ => Effect::StarShimmer { seed, speed: 1.0 },
            };
            shader(effect)
                .progress(progress.filter(|_| state == StudioRunState::Downloading))
                .absolute()
                .top_0()
                .left_0()
                .w(px(width))
                .h(px(height))
                .rounded(px(10.0))
        });
        base.relative()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.0))
            .text_color(theme.text_faint)
            .when_some(fill, |tile, fill| tile.child(fill))
            .when(state == StudioRunState::Failed, |tile| {
                tile.child("Generation failed")
            })
            .when_some(
                artifact_id.and_then(|id| self.artifact_focus_ring(id, theme)),
                |tile, ring| tile.child(ring),
            )
            .into_any_element()
    }

    fn artifact_focus_ring(
        &self,
        artifact_id: StudioArtifactId,
        theme: &Theme,
    ) -> Option<AnyElement> {
        let alpha = self.artifact_focus_alpha(artifact_id)?;
        Some(
            div()
                .absolute()
                .inset_0()
                .rounded(px(10.0))
                .border_2()
                .border_color(theme.text.opacity(0.88 * alpha))
                .into_any_element(),
        )
    }

    pub(super) fn feed_turns(&self) -> &[StudioTurnView] {
        self.conversation
            .as_ref()
            .map(|view| view.turns.as_slice())
            .unwrap_or(&[])
    }

    pub(super) fn rail_should_show(&self, container_width: f32) -> bool {
        rail::rail_visible(container_width) && self.feed_turns().len() >= 2
    }

    pub(super) fn feed_container_width(&self, window: &Window) -> f32 {
        // Never read ListState here: `render_feed_row` runs while the list
        // holds a mutable borrow, and `viewport_bounds()` would panic.
        if self.feed_width > 1.0 {
            self.feed_width
        } else {
            (f32::from(window.viewport_size().width) - crate::settings::SIDEBAR_DEFAULT).max(0.0)
        }
    }

    pub(super) fn reset_feed_list(&mut self) {
        self.feed_list.reset(0);
        self.feed_visible_rows = 0..0;
        self.feed_layout_sig = None;
        self.feed_columns = 0;
        self.scroll_task = None;
    }

    pub(super) fn feed_scroll_to_end(&self) {
        self.feed_list.scroll_to_end();
    }

    fn feed_content_width(&self, window: &Window) -> f32 {
        let rail_gutter = if self.rail_should_show(self.feed_container_width(window)) {
            STUDIO_RAIL_GUTTER
        } else {
            0.0
        };
        (self.feed_container_width(window) - FEED_PAD_X * 2.0 - rail_gutter).clamp(240.0, 1600.0)
    }

    fn feed_grid_metrics(&self, window: &Window) -> (f32, f32, f32) {
        let available = self.feed_content_width(window);
        let columns = grid_columns_sticky(available, self.feed_columns);
        let gap = if available < 520.0 { 12.0 } else { 16.0 };
        let tile_width =
            (available - gap * (columns.saturating_sub(1) as f32)) / columns.max(1) as f32;
        (tile_width, gap, available)
    }

    fn compute_feed_layout_sig(&self) -> FeedLayoutSig {
        let width = self.feed_width.max(1.0);
        let rail_gutter = if self.rail_should_show(width) {
            STUDIO_RAIL_GUTTER
        } else {
            0.0
        };
        let available = (width - FEED_PAD_X * 2.0 - rail_gutter).clamp(240.0, 1600.0);
        let columns = grid_columns_sticky(available, self.feed_columns);
        let gap = if available < 520.0 { 12.0 } else { 16.0 };
        let tile = (available - gap * (columns.saturating_sub(1) as f32)) / columns.max(1) as f32;
        FeedLayoutSig {
            tile_q: (tile / TILE_QUANT).round() as u32,
            columns,
            turns: self
                .feed_turns()
                .iter()
                .map(|turn| TurnLayoutSig {
                    id: turn.id,
                    expanded: self.expanded_prompts.contains(&turn.id),
                    slots: turn
                        .runs
                        .iter()
                        .map(|run| {
                            let (aw, ah) = run.display_aspect_ratio;
                            (
                                aw.min(u16::MAX as u32) as u16,
                                ah.min(u16::MAX as u32) as u16,
                                feed_output_slots(run).len() as u16,
                            )
                        })
                        .collect(),
                })
                .collect(),
        }
    }

    pub(super) fn sync_feed_list(&mut self) {
        let count = self.feed_turns().len();
        let sig = self.compute_feed_layout_sig();
        self.feed_columns = sig.columns;
        let old_count = self.feed_list.item_count();
        let old_sig = self.feed_layout_sig.replace(sig.clone());

        if count == 0 {
            if old_count != 0 {
                self.feed_list.reset(0);
            }
            return;
        }

        if old_count == 0 {
            self.feed_list.splice(0..0, count);
            self.feed_list.clone().measure_all();
            return;
        }

        if count != old_count {
            if count > old_count {
                self.feed_list
                    .splice(old_count..old_count, count - old_count);
            } else {
                self.feed_list.splice(count..old_count, 0);
            }
            remeasure_changed_feed_rows(&self.feed_list, old_sig.as_ref(), &sig);
            self.feed_list.clone().measure_all();
            return;
        }

        if old_sig.as_ref() != Some(&sig) {
            remeasure_changed_feed_rows(&self.feed_list, old_sig.as_ref(), &sig);
        }
    }

    pub(super) fn feed_ids_around_visible(&self, extra: usize) -> Vec<StudioArtifactId> {
        let turns = self.feed_turns();
        let visible = feed_visible_or_tail(self.feed_visible_rows.clone(), turns.len());
        let start = visible.start.saturating_sub(extra);
        let end = visible.end.saturating_add(extra).min(turns.len());
        feed_image_ids(turns, start..end)
    }

    /// The list's scroll handler is wheel/touch only. Scrollbar
    /// `set_offset_from_scrollbar` never updates `feed_visible_rows`, so a
    /// thumb scrub has to read the current top item back out of the list.
    pub(super) fn sync_feed_visible_rows(&mut self) {
        let list = &self.feed_list;
        let count = list.item_count();
        let top = list.logical_scroll_top().item_ix;
        let viewport_bottom = f32::from(list.viewport_bounds().bottom());
        let mut span = 0;
        if top < count && viewport_bottom > 0.0 {
            span = 1;
            let mut ix = top + 1;
            while ix < count {
                match list.bounds_for_item(ix) {
                    Some(bounds) if f32::from(bounds.top()) < viewport_bottom => {
                        span += 1;
                        ix += 1;
                    }
                    _ => break,
                }
            }
        }
        self.feed_visible_rows = feed_visible_from_top(top, count, span);
    }

    pub(super) fn request_visible_feed_images(&mut self, cx: &mut Context<Self>) {
        // Do not read ListState here. The wheel handler runs while the list
        // holds a mutable borrow — `item_count()` would panic.
        let visible = self.feed_ids_around_visible(0);
        let mut thumbs = Vec::new();
        for id in self.feed_ids_around_visible(FEED_PREFETCH_TURNS) {
            if !visible.contains(&id) {
                thumbs.push(id);
            }
        }
        self.image_protect = visible.iter().chain(thumbs.iter()).copied().collect();
        self.image_protect
            .extend(self.loading_images.iter().copied());
        self.request_images(visible, false, cx);
        self.request_images(thumbs, false, cx);
    }

    /// Smooth-scroll the feed so `turn_ix` sits at the viewport top — same
    /// 500ms ease-in-out timeline as the chat MessageRail. Item-space while
    /// the target is unmeasured; pixel-exact once `bounds_for_item` lands.
    pub(super) fn scroll_to_turn(&mut self, turn_ix: usize, cx: &mut Context<Self>) {
        self.scroll_to_turn_offset(turn_ix, 0.0, cx);
    }

    pub(super) fn scroll_to_turn_offset(
        &mut self,
        turn_ix: usize,
        offset: f32,
        cx: &mut Context<Self>,
    ) {
        let offset = offset.max(0.0);
        if motion::reduced_motion(cx) {
            self.feed_list.scroll_to(ListOffset {
                item_ix: turn_ix,
                offset_in_item: px(offset),
            });
            cx.notify();
            return;
        }
        self.scroll_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let total = motion::SCROLL_GLIDE.total().mul_f32(motion::speed_scale());
            let mut timeline = rail::GlideTimeline::new();
            let mut height_ema: Option<f32> = None;
            let frames = (total.as_millis() / 16) as usize + 90;
            for _ in 0..frames {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let raw = (started.elapsed().as_secs_f32() / total.as_secs_f32()).min(1.0);
                let eased = motion::SCROLL_GLIDE.curve.eval(raw);
                let frac = timeline.step(eased);
                let done = this.update(cx, |page, cx| {
                    let list = page.feed_list.clone();
                    if raw >= 1.0 {
                        list.scroll_to(ListOffset {
                            item_ix: turn_ix,
                            offset_in_item: px(offset),
                        });
                        cx.notify();
                        return true;
                    }
                    let viewport = f32::from(list.viewport_bounds().size.height);
                    let top = list.logical_scroll_top();
                    let top_height = list
                        .bounds_for_item(top.item_ix)
                        .map(|b| f32::from(b.size.height).max(1.0));
                    if viewport > 0.0 {
                        let bottom = f32::from(list.viewport_bounds().bottom());
                        let mut ix = top.item_ix;
                        let mut count = 0.0f32;
                        while let Some(b) = list.bounds_for_item(ix) {
                            if f32::from(b.top()) >= bottom {
                                break;
                            }
                            count += 1.0;
                            ix += 1;
                        }
                        if count > 0.0 {
                            let mean = viewport / count;
                            let ema = height_ema.get_or_insert(mean);
                            *ema += 0.5 * (mean - *ema);
                        }
                    }
                    if height_ema.is_none() {
                        height_ema = top_height;
                    }
                    let here = top.item_ix as f32
                        + top_height
                            .map(|h| (f32::from(top.offset_in_item) / h).clamp(0.0, 1.0))
                            .unwrap_or(0.0);
                    if turn_ix < top.item_ix {
                        let next = here - frac * (here - turn_ix as f32);
                        let step_px = (here - next) * height_ema.unwrap_or(0.0);
                        if step_px > 0.0 && step_px <= FEED_OVERDRAW_PX * 0.8 {
                            list.scroll_by(px(-step_px));
                            cx.notify();
                            return false;
                        }
                        let ix = (next.floor().max(0.0) as usize).min(top.item_ix);
                        let within = next - ix as f32;
                        let offset = if ix == top.item_ix {
                            top_height
                                .map(|h| (within * h).min(f32::from(top.offset_in_item)))
                                .unwrap_or(0.0)
                        } else {
                            within * height_ema.unwrap_or(0.0)
                        };
                        list.scroll_to(ListOffset {
                            item_ix: ix,
                            offset_in_item: px(offset),
                        });
                        cx.notify();
                        return false;
                    }
                    match list.bounds_for_item(turn_ix) {
                        Some(bounds) => {
                            let delta = f32::from(bounds.top()) + offset
                                - f32::from(list.viewport_bounds().top());
                            list.scroll_by(px(frac * delta));
                        }
                        None => {
                            let next = here + frac * (turn_ix as f32 - here);
                            let ix = (next.floor().max(0.0) as usize).min(turn_ix);
                            let within = next - ix as f32;
                            list.scroll_to(ListOffset {
                                item_ix: ix,
                                offset_in_item: px(within * height_ema.unwrap_or(0.0)),
                            });
                        }
                    }
                    cx.notify();
                    false
                });
                match done {
                    Ok(true) | Err(_) => return,
                    Ok(false) => {}
                }
            }
            this.update(cx, |page, cx| {
                page.feed_list.scroll_to(ListOffset {
                    item_ix: turn_ix,
                    offset_in_item: px(offset),
                });
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn render_conversation_feed(
        &mut self,
        window: &mut Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let has_turns = !self.feed_turns().is_empty();
        self.sync_feed_list();
        if has_turns {
            // Render is outside the list's layout borrow.
            self.sync_feed_visible_rows();
            self.request_visible_feed_images(cx);
        }
        let rail = self.render_studio_rail(window, theme, cx);
        let measure_entity = cx.weak_entity();
        let body = if has_turns {
            let list_element = list(self.feed_list.clone(), cx.processor(Self::render_feed_row))
                .flex_1()
                .size_full()
                .with_sizing_behavior(gpui::ListSizingBehavior::Auto);
            div()
                .id("studio-feed-scroll")
                .size_full()
                // Start the scroll viewport at the fade edge. Previously it
                // began below the whole band, so no painted row could ever
                // enter the EdgeFade ramp.
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .child(list_element)
                .into_any_element()
        } else {
            div()
                .id("studio-feed-scroll")
                .size_full()
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(crate::motion::fade_in(
                    "new-studio-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            crate::icons::icon(crate::icons::ZERON_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.text.opacity(0.2)),
                        )
                        .child(
                            div()
                                .mt(px(12.0))
                                .text_size(px(14.0))
                                .text_color(theme.text_muted.opacity(0.6))
                                .child("Describe an image below to begin"),
                        ),
                ))
                .into_any_element()
        };
        let top = self.feed_list.logical_scroll_top();
        let fade_top = top.item_ix > 0 || f32::from(top.offset_in_item) > 1.0;
        let fade_bottom = self.feed_list.is_scrolled_to_end() == Some(false);
        let feed =
            crate::edge_fade::edge_faded(Theme::TRANSCRIPT_FADE_BAND, fade_top, fade_bottom, body)
                .inset_top(Theme::TITLEBAR_HEIGHT)
                .band_top(Theme::TRANSCRIPT_FADE_BAND)
                // The transcript scrolls underneath the floating composer. Fade
                // it before the composer chrome instead of clipping the last row
                // against a hard edge.
                .band_bottom(112.0);
        let scrub = cx.weak_entity();
        div()
            .relative()
            .flex_1()
            .min_w_0()
            .h_full()
            .overflow_hidden()
            .child(
                canvas(
                    move |bounds, window, cx| {
                        let width = f32::from(bounds.size.width);
                        // Absolute children layout after the in-flow list, which
                        // still holds ListState. Only stash the width; remesure
                        // on the next render. Ignore collapsed/zero passes so
                        // we do not flip between "window fallback" and the pane.
                        let changed = measure_entity
                            .update(cx, |page, _| {
                                if width < 64.0 || (page.feed_width - width).abs() <= 0.5 {
                                    return false;
                                }
                                page.feed_width = width;
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
            .child(feed)
            .child(rail)
            .when(has_turns, |el| {
                el.child(
                    crate::scrollbar::overlay("studio-feed", &self.feed_list)
                        .inset_top(Theme::TITLEBAR_HEIGHT)
                        .on_scrub(move |_, cx| {
                            scrub
                                .update(cx, |page: &mut StudioPage, cx| {
                                    page.sync_feed_visible_rows();
                                    page.request_visible_feed_images(cx);
                                    cx.notify();
                                })
                                .ok();
                        }),
                )
            })
            .into_any_element()
    }

    fn render_feed_row(
        &mut self,
        turn_ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(turn) = self.feed_turns().get(turn_ix).cloned() else {
            return gpui::Empty.into_any_element();
        };
        let theme = Theme::of(cx).clone();
        let (tile_width, gap, content_width) = self.feed_grid_metrics(window);
        let last = turn_ix + 1 == self.feed_turns().len();
        let show_rail = self.rail_should_show(self.feed_container_width(window));
        let left_pad = if show_rail {
            FEED_PAD_X + STUDIO_RAIL_GUTTER
        } else {
            FEED_PAD_X
        };
        div()
            .w_full()
            .pl(px(left_pad))
            .pr(px(FEED_PAD_X))
            .pb(px(if last {
                STUDIO_COMPOSER_CLEARANCE
            } else {
                FEED_TURN_GAP
            }))
            .child(self.render_turn(
                turn_ix,
                &turn,
                tile_width,
                gap,
                content_width,
                &theme,
                window,
                cx,
            ))
            .into_any_element()
    }

    pub(super) fn render_turn(
        &mut self,
        turn_ix: usize,
        turn: &StudioTurnView,
        tile_width: f32,
        gap: f32,
        content_width: f32,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turn_for_prompt = turn.clone();
        let turn_for_fork = turn.clone();
        let is_latest = turn_ix + 1 == self.feed_turns().len();
        let turn_id = turn.id;
        let expanded = self.expanded_prompts.contains(&turn.id);
        let bubble_width = content_width
            .min(1600.0)
            .min(transcript::MAX_CONTENT_WIDTH * 0.8);
        let inner_width = (bubble_width - PROMPT_BUBBLE_PAD_X * 2.0).max(1.0);
        let clampable = prompt_exceeds_collapsed_lines(&turn.prompt, inner_width);
        let collapsed = clampable && !expanded;
        let mut grid = div().w_full().flex().flex_row().flex_wrap().gap(px(gap));
        for (run_ix, run) in turn.runs.iter().enumerate() {
            for (output_ix, artifact) in feed_output_slots(run) {
                grid = grid.child(self.render_tile(
                    turn_ix,
                    run_ix,
                    output_ix,
                    tile_width,
                    run.display_aspect_ratio,
                    run.state,
                    artifact,
                    run.progress,
                    theme,
                    window,
                    cx,
                ));
            }
        }
        let retry_runs = turn
            .runs
            .iter()
            .filter(|run| run.state == StudioRunState::Failed)
            .map(|run| {
                (
                    run.id,
                    run.error
                        .as_deref()
                        .is_some_and(|error| error.contains("may have completed")),
                )
            })
            .collect::<Vec<_>>();
        let show_more = clampable.then(|| {
            show_more_action(
                format!("studio-toggle-prompt-{}", turn_id.0),
                expanded,
                theme,
            )
            .on_click(cx.listener(move |page, _, _, cx| {
                page.toggle_prompt_expanded(turn_id, cx);
            }))
        });
        div()
            .id(SharedString::from(format!("studio-turn-{turn_ix}")))
            .w_full()
            .max_w(px(1600.0))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .w_full()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        // Agent-chat user bubble. Short prompts keep text as a
                        // direct child so the plate hugs. Long prompts take an
                        // explicit width and stack Show more inside — without
                        // that width a flex column collapses to the button.
                        div().w_full().flex().justify_end().child(
                            div()
                                .min_w_0()
                                .max_w(px(transcript::MAX_CONTENT_WIDTH * 0.8))
                                .when(clampable, |el| {
                                    el.w(px(bubble_width)).flex().flex_col().gap(px(6.0))
                                })
                                .bg(crate::theme::user_bubble_bg())
                                .rounded(px(Theme::BUBBLE_RADIUS))
                                .px(px(PROMPT_BUBBLE_PAD_X))
                                .py(px(10.0))
                                .text_size(px(14.0))
                                .line_height(px(PROMPT_LINE_HEIGHT))
                                .text_color(theme.text)
                                .when(!clampable, |el| {
                                    el.child(SharedString::from(turn.prompt.clone()))
                                })
                                .when(clampable, |el| {
                                    el.child(
                                        div()
                                            .w_full()
                                            .when(collapsed, |box_| {
                                                box_.max_h(px(PROMPT_LINE_HEIGHT
                                                    * PROMPT_COLLAPSED_LINES as f32))
                                                    .overflow_hidden()
                                            })
                                            .child(SharedString::from(turn.prompt.clone())),
                                    )
                                    .children(show_more)
                                }),
                        ),
                    )
                    .child(
                        div()
                            .w_full()
                            .flex()
                            .flex_wrap()
                            .justify_end()
                            .items_center()
                            .gap(px(10.0))
                            .when_some(
                                super::cost::turn_quote(turn)
                                    .as_ref()
                                    .map(super::cost::format_quote),
                                |row, cost| {
                                    row.child(
                                        div()
                                            .flex_none()
                                            .text_size(px(11.0))
                                            .text_color(theme.text_muted.opacity(0.7))
                                            .child(SharedString::from(cost)),
                                    )
                                },
                            )
                            .child(
                                turn_action(
                                    format!("studio-use-prompt-{turn_ix}"),
                                    "Use prompt",
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |page, _, _, cx| page.use_prompt(&turn_for_prompt, cx),
                                )),
                            )
                            .child(
                                turn_action(format!("studio-fork-{turn_ix}"), "Fork", theme)
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.fork_from(&turn_for_fork, cx)
                                    })),
                            )
                            .children(retry_runs.into_iter().map(|(run_id, retry_anyway)| {
                                let fade = format!("studio-retry-{}", run_id.0);
                                turn_action(
                                    fade.clone(),
                                    if retry_anyway {
                                        "Retry anyway"
                                    } else {
                                        "Retry"
                                    },
                                    theme,
                                )
                                .text_color(motion::hover_blend(
                                    &fade,
                                    theme.danger.opacity(0.85),
                                    theme.danger,
                                ))
                                .on_click(cx.listener(
                                    move |page, _, _, cx| page.retry(run_id, retry_anyway, cx),
                                ))
                            })),
                    ),
            )
            .child(grid)
            .when(!is_latest, |el| {
                let turn_for_more = turn.clone();
                el.child(
                    turn_action(
                        format!("studio-generate-more-{turn_ix}"),
                        "Generate more",
                        theme,
                    )
                    .on_click(
                        cx.listener(move |page, _, _, cx| page.generate_more(&turn_for_more, cx)),
                    ),
                )
            })
            .into_any_element()
    }

    /// Left-edge tick rail for the conversation feed — same chrome as the
    /// chat MessageRail, with a Studio-specific hover card.
    ///
    /// Active tick is the turn that occupies the most of the unobstructed
    /// reading band, not the viewport-top row: Studio turns are tall, so a
    /// sliver of the previous turn at the clip top must not keep its tick
    /// lit. Pinned-to-end always highlights the last tick.
    pub(super) fn render_studio_rail(
        &mut self,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if !self.rail_should_show(self.feed_container_width(window)) {
            return gpui::Empty.into_any_element();
        }
        let ticks = studio_rail_ticks(self.feed_turns());
        if ticks.len() < 2 {
            return gpui::Empty.into_any_element();
        }
        let n = ticks.len();
        let list = &self.feed_list;
        let top_row = list.logical_scroll_top().item_ix;
        let viewport = list.viewport_bounds();
        let vp_bottom = f32::from(viewport.bottom());
        // `is_scrolled_to_end` is the usual pin; also treat a last row whose
        // bottom has reached the clip as pinned — wheel/scrollbar can sit a
        // pixel short of the official end while the last turn is fully in
        // view and a previous turn still owns the clip top.
        let last_flush = list
            .bounds_for_item(n - 1)
            .is_some_and(|bounds| f32::from(bounds.bottom()) <= vp_bottom + 8.0);
        let at_end = list.is_scrolled_to_end() == Some(true) || top_row >= n || last_flush;
        let reading_top = f32::from(viewport.top());
        let reading_bottom = (vp_bottom - STUDIO_READING_BOTTOM_INSET).max(reading_top + 1.0);
        let mut spans = Vec::with_capacity(n);
        let mut measured = true;
        for i in 0..n {
            match list.bounds_for_item(i) {
                Some(bounds) => {
                    let top = f32::from(bounds.top());
                    let mut bottom = f32::from(bounds.bottom());
                    // Last row includes the composer runway — score the
                    // prompt + images, not the empty pad behind the card.
                    if i + 1 == n {
                        bottom = (bottom - STUDIO_COMPOSER_CLEARANCE).max(top);
                    }
                    spans.push(TurnSpan { top, bottom });
                }
                None => {
                    measured = false;
                    break;
                }
            }
        }
        let active = if measured {
            studio_active_tick(&spans, reading_top, reading_bottom, at_end)
        } else if at_end {
            Some(n - 1)
        } else {
            let tick_rows: Vec<usize> = ticks.iter().map(|tick| tick.turn_ix).collect();
            rail::active_tick(&tick_rows, top_row)
        };
        let hover = self.rail_hover;
        let viewport_h = f32::from(self.feed_list.viewport_bounds().size.height);
        let capacity = rail::rail_slots(if viewport_h > 0.0 { viewport_h } else { 600.0 });
        let buckets = rail::tick_buckets(ticks.len(), capacity);
        let bucket_n = buckets.len();
        let active_bucket = active.and_then(|ix| rail::bucket_of(&buckets, ix));
        let now = Utc::now();

        rail::rail_stack()
            .children(buckets.into_iter().enumerate().map(|(ix, (start, end))| {
                let rep = active.filter(|&a| a >= start && a < end).unwrap_or(start);
                let tick = ticks[rep].clone();
                let bucket_len = end - start;
                let is_active = active_bucket == Some(ix);
                let is_hovered = hover == Some(ix);
                let prompt = rail::truncate_preview(&tick.prompt, rail::PREVIEW_PROMPT_CHARS);
                let models = format_studio_models(&tick.models);
                let sent = format_studio_tick_time(tick.created_at, now, &chrono::Local);
                let card: Option<AnyElement> = is_hovered.then(|| {
                    let card = popover::popover_card(theme)
                        .w(px(280.0))
                        .p(px(Theme::SPACE_SM))
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.text)
                                .child(SharedString::from(prompt)),
                        )
                        .when(!models.is_empty(), |el| {
                            el.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(models)),
                            )
                        })
                        .when_some(tick.cost.clone(), |el, cost| {
                            el.child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(theme.text_muted)
                                    .child(SharedString::from(cost)),
                            )
                        })
                        .child(
                            div()
                                .text_size(px(10.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child(SharedString::from(sent)),
                        )
                        .when(bucket_len > 1, |el| {
                            el.child(
                                div()
                                    .text_size(px(10.0))
                                    .text_color(theme.text_muted.opacity(0.7))
                                    .child(SharedString::from(format!("{bucket_len} turns"))),
                            )
                        });
                    crate::frost::frosted(12.0, crate::frost::MENU_BLUR, card).into_any_element()
                });
                let turn_ix = tick.turn_ix;
                rail::rail_tick(
                    ("studio-rail-tick", ix),
                    ix,
                    bucket_n,
                    rail::rail_bar_width(is_hovered),
                    rail::rail_bar_color(theme, is_active, is_hovered),
                    card,
                )
                .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                    rail::apply_rail_hover(&mut this.rail_hover, ix, *hovered);
                    cx.notify();
                }))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.scroll_to_turn(turn_ix, cx);
                }))
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, TimeZone, Utc};
    use std::collections::BTreeMap;
    use zeron_proto::StudioTurnView;

    #[test]
    fn feed_breakpoints_follow_the_plan() {
        assert_eq!(grid_columns(519.0), 1);
        assert_eq!(grid_columns(520.0), 2);
        assert_eq!(grid_columns(899.0), 2);
        assert_eq!(grid_columns(900.0), 3);
        assert_eq!(grid_columns(1239.0), 3);
        assert_eq!(grid_columns(1240.0), 4);
    }

    #[test]
    fn grid_columns_sticky_ignores_wobble_around_cuts() {
        assert_eq!(grid_columns_sticky(900.0, 2), 2);
        assert_eq!(grid_columns_sticky(931.0, 2), 2);
        assert_eq!(grid_columns_sticky(932.0, 2), 3);
        assert_eq!(grid_columns_sticky(899.0, 3), 3);
        assert_eq!(grid_columns_sticky(868.0, 3), 3);
        assert_eq!(grid_columns_sticky(867.0, 3), 2);
        assert_eq!(grid_columns_sticky(1240.0, 3), 3);
        assert_eq!(grid_columns_sticky(1272.0, 3), 4);
        assert_eq!(grid_columns_sticky(1208.0, 4), 4);
        assert_eq!(grid_columns_sticky(1207.0, 4), 3);
        assert_eq!(grid_columns_sticky(900.0, 0), 3);
    }
    fn test_model(display_name: &str) -> zeron_studio::MediaModel {
        zeron_studio::MediaModel {
            provider_id: "venice".into(),
            id: display_name.into(),
            display_name: display_name.into(),
            description: None,
            operation: zeron_studio::MediaOperation::TextToImage,
            output_kind: zeron_studio::MediaKind::Image,
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
        }
    }

    fn test_artifact(output_position: u32) -> zeron_proto::StudioArtifactView {
        zeron_proto::StudioArtifactView {
            id: zeron_studio::StudioArtifactId::new(),
            output_position,
            media_kind: zeron_studio::MediaKind::Image,
            mime_type: "image/png".into(),
            size_bytes: 1,
            width: Some(1),
            height: Some(1),
            duration_seconds: None,
            metadata: serde_json::Value::Null,
            created_at: Utc::now(),
        }
    }

    fn test_run(display_name: &str, output_count: u32) -> zeron_proto::StudioRunView {
        zeron_proto::StudioRunView {
            id: zeron_studio::StudioRunId::new(),
            position: 0,
            provider_id: "venice".into(),
            model: test_model(display_name),
            controls: BTreeMap::new(),
            output_count,
            display_aspect_ratio: (1, 1),
            state: StudioRunState::Succeeded,
            progress: None,
            error: None,
            quote: None,
            artifacts: Vec::new(),
        }
    }

    fn test_turn(
        prompt: &str,
        created_at: DateTime<Utc>,
        runs: Vec<zeron_proto::StudioRunView>,
    ) -> StudioTurnView {
        StudioTurnView {
            id: zeron_studio::StudioTurnId::new(),
            position: 0,
            prompt: prompt.into(),
            source_turn_id: None,
            batch_id: zeron_studio::StudioBatchId::new(),
            runs,
            created_at,
        }
    }

    fn span(top: f32, bottom: f32) -> TurnSpan {
        TurnSpan { top, bottom }
    }

    #[test]
    fn studio_active_tick_empty_is_none() {
        assert_eq!(studio_active_tick(&[], 0.0, 800.0, false), None);
        assert_eq!(studio_active_tick(&[], 0.0, 800.0, true), None);
    }

    #[test]
    fn studio_active_tick_at_end_is_always_last() {
        // Previous turn still owns the clip top (the screenshot case): a
        // 7-image turn peeks into the reading band, but the list is pinned
        // to the bottom so the last tick must light.
        let spans = [span(-1600.0, 180.0), span(180.0, 700.0)];
        assert_eq!(studio_active_tick(&spans, 0.0, 680.0, true), Some(1));
        let three = [span(0.0, 400.0), span(400.0, 800.0), span(800.0, 1100.0)];
        assert_eq!(studio_active_tick(&three, 200.0, 880.0, true), Some(2));
    }

    #[test]
    fn studio_active_tick_picks_the_turn_that_fills_the_reading_band() {
        // Leftover sliver of the previous turn at the top — last turn fills
        // the band, so it is the plausible tick even before `at_end`.
        let spans = [span(-400.0, 180.0), span(180.0, 800.0)];
        assert_eq!(studio_active_tick(&spans, 0.0, 680.0, false), Some(1));

        // Still mostly the first turn.
        let early = [span(-100.0, 500.0), span(500.0, 1000.0)];
        assert_eq!(studio_active_tick(&early, 0.0, 680.0, false), Some(0));

        // Crossed over: second turn now occupies more of the band.
        let crossed = [span(-400.0, 250.0), span(250.0, 900.0)];
        assert_eq!(studio_active_tick(&crossed, 0.0, 680.0, false), Some(1));
    }

    #[test]
    fn studio_active_tick_tie_prefers_the_later_turn() {
        let spans = [span(0.0, 340.0), span(340.0, 680.0)];
        assert_eq!(studio_active_tick(&spans, 0.0, 680.0, false), Some(1));
    }

    #[test]
    fn studio_active_tick_middle_of_three() {
        let spans = [span(-300.0, 100.0), span(100.0, 600.0), span(600.0, 1100.0)];
        assert_eq!(studio_active_tick(&spans, 0.0, 680.0, false), Some(1));
    }

    #[test]
    fn studio_active_tick_falls_back_when_nothing_intersects() {
        // Everything still below the band (short first turn, reading line
        // sits in the title-adjacent pad) → first tick.
        let below = [span(800.0, 1200.0), span(1200.0, 1600.0)];
        assert_eq!(studio_active_tick(&below, 0.0, 680.0, false), Some(0));
        // Everything already above the band, not marked at-end → last.
        let above = [span(-2000.0, -1200.0), span(-1200.0, -200.0)];
        assert_eq!(studio_active_tick(&above, 0.0, 680.0, false), Some(1));
    }

    #[test]
    fn studio_rail_ticks_map_turns_to_models_and_variation_counts() {
        let now = Utc::now();
        let turns = vec![
            test_turn(
                "a fox in snow",
                now,
                vec![test_run("Flux", 4), test_run("Kling", 2)],
            ),
            test_turn("second prompt", now, vec![test_run("Flux", 1)]),
        ];
        let ticks = studio_rail_ticks(&turns);
        assert_eq!(ticks.len(), 2);
        assert_eq!(ticks[0].turn_ix, 0);
        assert_eq!(ticks[0].prompt, "a fox in snow");
        assert_eq!(
            ticks[0].models,
            vec![("Flux".into(), 4), ("Kling".into(), 2)]
        );
        assert_eq!(ticks[1].models, vec![("Flux".into(), 1)]);
        assert!(studio_rail_ticks(&[]).is_empty());
    }

    #[test]
    fn studio_rail_ticks_merge_repeated_generate_more_runs() {
        let now = Utc::now();
        let turns = vec![test_turn(
            "a fox in snow",
            now,
            vec![
                test_run("Flux", 4),
                test_run("Flux", 4),
                test_run("Kling", 2),
            ],
        )];
        let ticks = studio_rail_ticks(&turns);
        assert_eq!(
            ticks[0].models,
            vec![("Flux".into(), 8), ("Kling".into(), 2)]
        );
    }

    #[test]
    fn prompt_clamp_counts_blank_and_wrapped_lines() {
        // 80px inner ≈ 10 chars/line at the 7.4px advance.
        let inner = 80.0;
        assert_eq!(prompt_visual_lines("short", inner), 1);
        assert!(!prompt_exceeds_collapsed_lines("short", inner));
        assert!(!prompt_exceeds_collapsed_lines("one\ntwo\nthree", inner));
        assert!(prompt_exceeds_collapsed_lines(
            "one\ntwo\nthree\nfour",
            inner
        ));
        assert!(prompt_exceeds_collapsed_lines("one\n\nthree\nfour", inner));
        assert!(prompt_exceeds_collapsed_lines(
            "this line is definitely longer than ten glyphs",
            inner
        ));
        assert_eq!(prompt_visual_lines("abcdefghijabcdefghij", inner), 2);
        assert_eq!(prompt_visual_lines("a\n\nb", inner), 3);
        // A short studio prompt at the chat-bubble inner width stays one row.
        let chat_inner = transcript::MAX_CONTENT_WIDTH * 0.8 - PROMPT_BUBBLE_PAD_X * 2.0;
        assert!(!prompt_exceeds_collapsed_lines("cute dog", chat_inner));
        assert_eq!(prompt_visual_lines("cute dog", chat_inner), 1);
    }

    #[test]
    fn inspector_prompt_clamps_after_ten_visual_lines() {
        // 320 sidebar − 18px padding each side − 24px copy − 8px gap.
        let inner = 252.0;
        let advance = PROMPT_AVG_CHAR_ADVANCE * (12.0 / 14.0);
        let ten = ["line"; 10].join("\n");
        let eleven = format!("{ten}\nline");
        assert!(!prompt_exceeds_lines(&ten, inner, advance, 10));
        assert!(prompt_exceeds_lines(&eleven, inner, advance, 10));
        assert!(!prompt_exceeds_lines("short", inner, advance, 10));
    }

    #[test]
    fn conversation_image_count_is_artifacts_not_turns_or_slots() {
        let now = Utc::now();
        assert_eq!(conversation_image_count(&[]), 0);

        let empty_turn = test_turn("prompt", now, vec![test_run("Flux", 4)]);
        assert_eq!(conversation_image_count(&[empty_turn]), 0);

        let mut run = test_run("Flux", 4);
        run.artifacts = vec![test_artifact(0), test_artifact(2)];
        let mut other = test_run("Kling", 1);
        other.artifacts = vec![test_artifact(0)];
        let mut video = test_artifact(1);
        video.media_kind = zeron_studio::MediaKind::Video;
        other.artifacts.push(video);
        let turns = vec![
            test_turn("first", now, vec![run]),
            test_turn("second", now, vec![other]),
        ];
        assert_eq!(conversation_image_count(&turns), 3);
    }

    #[test]
    fn turn_index_for_artifact_finds_the_owning_turn() {
        let now = Utc::now();
        let first = test_artifact(0);
        let second = test_artifact(0);
        let missing = test_artifact(0);
        let mut early = test_run("Flux", 1);
        early.artifacts = vec![first.clone()];
        let mut later = test_run("Kling", 1);
        later.artifacts = vec![second.clone()];
        let turns = vec![
            test_turn("first", now, vec![early]),
            test_turn("second", now, vec![later]),
        ];
        assert_eq!(turn_index_for_artifact(&turns, first.id), Some(0));
        assert_eq!(turn_index_for_artifact(&turns, second.id), Some(1));
        assert_eq!(turn_index_for_artifact(&turns, missing.id), None);
    }

    #[test]
    fn artifact_feed_target_lands_on_the_tile_row() {
        let now = Utc::now();
        let first = test_artifact(0);
        let second = test_artifact(0);
        let mut early = test_run("Flux", 1);
        early.artifacts = vec![first.clone()];
        early.display_aspect_ratio = (1, 1);
        let mut later = test_run("Flux", 1);
        later.artifacts = vec![second.clone()];
        later.display_aspect_ratio = (1, 1);
        let turns = vec![test_turn("short", now, vec![early, later])];
        let first_target = artifact_feed_target(&turns, first.id, 800.0, 200.0, 16.0, 1).unwrap();
        let second_target = artifact_feed_target(&turns, second.id, 800.0, 200.0, 16.0, 1).unwrap();
        assert_eq!(first_target.turn_ix, 0);
        assert_eq!(second_target.turn_ix, 0);
        assert!(
            second_target.offset > first_target.offset,
            "second tile should sit on the next row, got {} then {}",
            first_target.offset,
            second_target.offset
        );
        assert!((second_target.offset - first_target.offset - 216.0).abs() < 0.5);
    }

    #[test]
    fn artifact_focus_ring_holds_then_fades() {
        assert_eq!(artifact_focus_alpha(Duration::ZERO), Some(1.0));
        assert_eq!(artifact_focus_alpha(ARTIFACT_FOCUS_HOLD), Some(1.0));
        let mid = artifact_focus_alpha(ARTIFACT_FOCUS_HOLD + ARTIFACT_FOCUS_FADE / 2).unwrap();
        assert!(mid > 0.0 && mid < 1.0, "mid fade was {mid}");
        assert_eq!(
            artifact_focus_alpha(ARTIFACT_FOCUS_HOLD + ARTIFACT_FOCUS_FADE),
            None
        );
    }

    #[test]
    fn studio_model_line_names_variations_per_model() {
        assert_eq!(format_studio_models(&[]), "");
        assert_eq!(
            format_studio_models(&[("Flux".into(), 1)]),
            "Flux · 1 variation"
        );
        assert_eq!(
            format_studio_models(&[("Flux".into(), 4)]),
            "Flux · 4 variations"
        );
        assert_eq!(
            format_studio_models(&[("Flux".into(), 4), ("Kling".into(), 2)]),
            "Flux · 4, Kling · 2"
        );
    }

    #[test]
    fn succeeded_runs_omit_deleted_output_slots() {
        let mut run = test_run("Flux", 2);
        let kept = test_artifact(1);
        let kept_id = kept.id;
        run.artifacts = vec![kept];
        assert_eq!(feed_output_slots(&run), vec![(1, Some(kept_id))]);

        run.artifacts.clear();
        assert!(feed_output_slots(&run).is_empty());
    }

    #[test]
    fn in_flight_and_failed_runs_keep_empty_slots() {
        let mut run = test_run("Flux", 2);
        run.state = StudioRunState::Running;
        assert_eq!(feed_output_slots(&run), vec![(0, None), (1, None)]);

        run.state = StudioRunState::Failed;
        run.output_count = 1;
        assert_eq!(feed_output_slots(&run), vec![(0, None)]);
    }

    #[test]
    fn feed_visible_or_tail_prefers_reported_range_else_latest_turns() {
        assert_eq!(feed_visible_or_tail(0..0, 10), 7..10);
        assert_eq!(feed_visible_or_tail(0..0, 2), 0..2);
        assert_eq!(feed_visible_or_tail(0..0, 0), 0..0);
        assert_eq!(feed_visible_or_tail(3..6, 10), 3..6);
        assert_eq!(feed_visible_or_tail(8..20, 10), 8..10);
    }

    #[test]
    fn feed_visible_from_top_covers_scrollbar_jumps() {
        assert_eq!(feed_visible_from_top(0, 0, 2), 0..0);
        assert_eq!(feed_visible_from_top(3, 10, 2), 3..5);
        assert_eq!(feed_visible_from_top(9, 10, 3), 9..10);
        // scroll_to_end anchors past the last item.
        assert_eq!(feed_visible_from_top(10, 10, 3), 7..10);
        assert_eq!(feed_visible_from_top(0, 4, 0), 0..1);
    }

    #[test]
    fn feed_image_ids_collect_only_images_in_range() {
        let now = Utc::now();
        let mut first = test_run("Flux", 2);
        first.artifacts = vec![test_artifact(0), test_artifact(1)];
        let mut second = test_run("Flux", 1);
        second.artifacts = vec![test_artifact(0)];
        let mut video = test_artifact(0);
        video.media_kind = zeron_studio::MediaKind::Video;
        let mut third = test_run("Kling", 1);
        third.artifacts = vec![video];
        let turns = vec![
            test_turn("one", now, vec![first]),
            test_turn("two", now, vec![second]),
            test_turn("three", now, vec![third]),
        ];
        assert_eq!(feed_image_ids(&turns, 0..1).len(), 2);
        assert_eq!(feed_image_ids(&turns, 1..2).len(), 1);
        assert!(feed_image_ids(&turns, 2..3).is_empty());
        assert_eq!(feed_image_ids(&turns, 0..3).len(), 3);
        assert!(feed_image_ids(&turns, 9..12).is_empty());
    }

    #[test]
    fn studio_tick_time_pairs_sidebar_relative_with_absolute_clock() {
        let tz = chrono::FixedOffset::west_opt(7 * 3600).unwrap();
        let then = tz
            .with_ymd_and_hms(2026, 7, 1, 15, 45, 0)
            .unwrap()
            .with_timezone(&Utc);
        let now = then + chrono::TimeDelta::minutes(5);
        assert_eq!(
            format_studio_tick_time(then, now, &tz),
            "5m · Jul 1, 3:45 PM"
        );
        let just_now = then + chrono::TimeDelta::seconds(10);
        assert_eq!(
            format_studio_tick_time(then, just_now, &tz),
            "now · Jul 1, 3:45 PM"
        );
    }
}
