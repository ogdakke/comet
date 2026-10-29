//! Studio's feed rail chrome: fixed-footprint tick stack (shadcn Transcript
//! Outline). Chat's own rail moved to a scrolling, evenly spread stack in
//! `crate::rail`; Studio keeps the compact bucketed outline.

use gpui::{AnyElement, Div, ElementId, Hsla, Stateful, div, prelude::*, px};

use crate::theme::Theme;

/// Preview text cap (grapheme-unaware char cut is fine for a preview card).
pub const PREVIEW_PROMPT_CHARS: usize = 160;

/// Hit rows absorb the nearer half of each gap so the stack has no dead space.
pub const TICK_SLOT: f32 = 10.0;
pub const TICK_GAP: f32 = 3.0;
/// Vertical breathing room kept clear above/below the tick stack.
pub const RAIL_V_MARGIN: f32 = 24.0;

/// Hard cap on visible ticks — the always-compact shadcn outline (its demo
/// trigger holds a small fixed stack of micro-marks regardless of window
/// height), not "fill the viewport, then condense".
pub const MAX_RAIL_TICKS: usize = 12;

/// How many tick slots fit in a rail of `height` px (always ≥ 1).
pub fn rail_capacity(height: f32) -> usize {
    let usable = (height - 2.0 * RAIL_V_MARGIN).max(TICK_SLOT);
    (((usable + TICK_GAP) / (TICK_SLOT + TICK_GAP)).floor() as usize).max(1)
}

/// Slots the rail actually uses: what fits the viewport, hard-capped at
/// [`MAX_RAIL_TICKS`] so the outline stays compact even on tall windows.
pub fn rail_slots(height: f32) -> usize {
    rail_capacity(height).min(MAX_RAIL_TICKS)
}

/// Hit-row height for tick `ix` of `n` so the gap between visual slots
/// belongs to the nearer tick.
pub fn tick_hit_height(ix: usize, n: usize) -> f32 {
    if n <= 1 {
        return if n == 0 { 0.0 } else { TICK_SLOT };
    }
    let half = TICK_GAP / 2.0;
    if ix == 0 || ix + 1 == n {
        TICK_SLOT + half
    } else {
        TICK_SLOT + TICK_GAP
    }
}

/// Top offset of the visual [`TICK_SLOT`] inside [`tick_hit_height`], so the
/// 2px bar stays where the old gapped layout painted it.
pub fn tick_hit_bar_offset(ix: usize, n: usize) -> f32 {
    if n <= 1 || ix == 0 {
        0.0
    } else {
        TICK_GAP / 2.0
    }
}

/// Update rail hover from a tick's `on_hover`.
///
/// Hit rows now share an edge, so sliding to the next tick can fire
/// `enter(next)` then `leave(prev)` in that order. Clearing on every leave
/// would drop the hover we just set (cards appear going up, vanish going
/// down). Only the tick that currently owns hover may clear it.
pub fn apply_rail_hover(current: &mut Option<usize>, ix: usize, hovered: bool) {
    if hovered {
        *current = Some(ix);
    } else if *current == Some(ix) {
        *current = None;
    }
}

pub fn rail_bar_width(hovered: bool) -> f32 {
    if hovered { 20.0 } else { 12.0 }
}

pub fn rail_bar_color(theme: &Theme, active: bool, hovered: bool) -> Hsla {
    if active || hovered {
        theme.text.opacity(0.8)
    } else {
        crate::theme::ink(0.16)
    }
}

/// Column that holds rail ticks. No gap — each tick's hit row absorbs the
/// nearer half of the inter-tick space.
pub fn rail_stack() -> Div {
    div()
        .absolute()
        .left(px(16.0))
        .top_0()
        .bottom_0()
        .w(px(26.0))
        .flex()
        .flex_col()
        .items_start()
        .justify_center()
}

/// Visual + hit chrome for one tick. Hover and click attach on the returned
/// row; `card` is the already-frosted preview.
pub fn rail_tick(
    id: impl Into<ElementId>,
    ix: usize,
    n: usize,
    bar_width: f32,
    bar_color: impl Into<Hsla>,
    card: Option<AnyElement>,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .h(px(tick_hit_height(ix, n)))
        .w_full()
        .cursor_pointer()
        .child(
            div()
                .absolute()
                .top(px(tick_hit_bar_offset(ix, n)))
                .left_0()
                .h(px(TICK_SLOT))
                .w_full()
                .flex()
                .items_center()
                .child(
                    div()
                        .h(px(2.0))
                        .w(px(bar_width))
                        .rounded(px(1.0))
                        .bg(bar_color.into()),
                )
                .when_some(card, |el, card| {
                    el.child(gpui::deferred(
                        gpui::anchored()
                            .anchor(gpui::Anchor::LeftCenter)
                            .snap_to_window_with_margin(px(8.0))
                            .child(div().pl(px(26.0)).child(card)),
                    ))
                }),
        )
}

/// shadcn's Transcript Outline keeps the always-visible rail a FIXED footprint
/// (tiny marks in a fixed-size trigger; the full turn list lives behind it) —
/// it never grows with the conversation. Same idea here: when prompts
/// outnumber the slots that fit the viewport, ticks become evenly-sized
/// BUCKETS over the conversation (a downsampled minimap) instead of
/// overflowing. Returns each bucket's `[start, end)` tick range; with
/// `n <= capacity` every bucket is a single tick — the identity, i.e. the
/// old per-prompt rail.
pub fn tick_buckets(n: usize, capacity: usize) -> Vec<(usize, usize)> {
    if n == 0 {
        return Vec::new();
    }
    let cap = capacity.clamp(1, n);
    (0..cap).map(|k| (k * n / cap, (k + 1) * n / cap)).collect()
}

/// The bucket containing tick `ix` (for active/hover mapping).
pub fn bucket_of(buckets: &[(usize, usize)], ix: usize) -> Option<usize> {
    buckets.iter().position(|&(s, e)| ix >= s && ix < e)
}

/// Char-cap a preview with an ellipsis. Whitespace runs (including newlines —
/// prompts and replies are free text) collapse to single spaces first: the
/// preview card's title is a one-line surface (message-rail.tsx line-clamp-1).
pub fn truncate_preview(text: &str, max_chars: usize) -> String {
    let flat = crate::transcript::single_line(text);
    if flat.chars().count() <= max_chars {
        return flat;
    }
    let cut: String = flat.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{}…", cut.trim_end())
}
