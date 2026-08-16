//! Conversation feed: tiles, turns, and the tick rail.

use std::time::{Duration, Instant};

use chrono::{DateTime, TimeZone, Utc};
use gpui::{AnyElement, Context, ObjectFit, Point, SharedString, Window, div, img, prelude::*, px};
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

use super::StudioEvent;
use super::page::StudioPage;

/// Scroll runway below the final Studio turn. The composer floats 18px above
/// the viewport and is 191px tall at its largest first-release configuration;
/// the remaining space keeps the last image clear of the glass card.
pub(super) const STUDIO_COMPOSER_CLEARANCE: f32 = 256.0;
/// Extra left inset so the tick rail (16px + 20px hover bar) does not cover
/// the prompt header. Matches the chat transcript's wide-gutter band.
pub(super) const STUDIO_RAIL_GUTTER: f32 = 28.0;
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

fn studio_rail_ticks(turns: &[StudioTurnView]) -> Vec<StudioRailTick> {
    turns
        .iter()
        .enumerate()
        .map(|(turn_ix, turn)| StudioRailTick {
            turn_ix,
            prompt: turn.prompt.clone(),
            models: turn
                .runs
                .iter()
                .map(|run| (run.model.display_name.clone(), run.output_count))
                .collect(),
            created_at: turn.created_at,
            cost: super::cost::turn_quote(turn)
                .as_ref()
                .map(super::cost::format_quote),
        })
        .collect()
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

/// Visual rows a prompt would occupy at `inner_width` (bubble minus padding).
/// Newlines always take a row, including blank ones.
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

/// Quiet text action under a prompt bubble (`Use prompt`, `Show more`).
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
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let height = width * aspect.1 as f32 / aspect.0.max(1) as f32;
        let base = div()
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
            && let Some(image) = self.images.get(&id)
        {
            let conversation_id = self.selected_conversation;
            return base
                .cursor_pointer()
                .on_click(cx.listener(move |page, _, window, cx| {
                    if let Some(index) =
                        page.artifact_sequence().iter().position(|item| *item == id)
                    {
                        page.select_artifact_index(index, cx);
                    } else {
                        page.selected_artifact = Some(id);
                        page.reset_lightbox_viewer();
                        if let Some(conversation_id) = conversation_id {
                            cx.emit(StudioEvent::OpenArtifact {
                                conversation_id,
                                artifact_id: id,
                            });
                        }
                        cx.notify();
                    }
                    window.focus(&page.focus, cx);
                }))
                .child(crate::motion::fade_quick(
                    SharedString::from(format!("studio-image-reveal-{}", id.0)),
                    img(image.clone())
                        .size_full()
                        .rounded(px(10.0))
                        .object_fit(ObjectFit::Contain),
                ))
                .into_any_element();
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
            .into_any_element()
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
        let measured = f32::from(self.feed_scroll.bounds().size.width);
        if measured > 0.0 {
            measured
        } else {
            (f32::from(window.viewport_size().width) - crate::settings::SIDEBAR_DEFAULT).max(0.0)
        }
    }

    /// Smooth-scroll the feed so `turn_ix` sits at the viewport top — same
    /// 500ms ease-in-out timeline as the chat MessageRail.
    pub(super) fn scroll_to_turn(&mut self, turn_ix: usize, cx: &mut Context<Self>) {
        if motion::reduced_motion(cx) {
            self.feed_scroll.scroll_to_top_of_item(turn_ix);
            cx.notify();
            return;
        }
        if self.feed_scroll.bounds_for_item(turn_ix).is_none() {
            self.feed_scroll.scroll_to_top_of_item(turn_ix);
            cx.notify();
            return;
        }
        self.scroll_task = Some(cx.spawn(async move |this, cx| {
            let started = Instant::now();
            let total = motion::SCROLL_GLIDE.total().mul_f32(motion::speed_scale());
            let mut timeline = rail::GlideTimeline::new();
            let frames = (total.as_millis() / 16) as usize + 90;
            for _ in 0..frames {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let raw = (started.elapsed().as_secs_f32() / total.as_secs_f32()).min(1.0);
                let eased = motion::SCROLL_GLIDE.curve.eval(raw);
                let frac = timeline.step(eased);
                let done = this.update(cx, |page, cx| {
                    if raw >= 1.0 {
                        page.feed_scroll.scroll_to_top_of_item(turn_ix);
                        cx.notify();
                        return true;
                    }
                    let here = f32::from(page.feed_scroll.offset().y);
                    let target = page
                        .feed_scroll
                        .bounds_for_item(turn_ix)
                        .map(|bounds| {
                            let raw_target =
                                f32::from(page.feed_scroll.bounds().top() - bounds.top());
                            let max_y = f32::from(page.feed_scroll.max_offset().y);
                            raw_target.clamp(-max_y, 0.0)
                        })
                        .unwrap_or(here);
                    page.feed_scroll.set_offset(Point {
                        x: px(0.0),
                        y: px(here + frac * (target - here)),
                    });
                    cx.notify();
                    false
                });
                match done {
                    Ok(true) | Err(_) => return,
                    Ok(false) => {}
                }
            }
            this.update(cx, |page, cx| {
                page.feed_scroll.scroll_to_top_of_item(turn_ix);
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn render_feed(
        &mut self,
        window: &Window,
        theme: &Theme,
        show_rail: bool,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let turns = self
            .conversation
            .clone()
            .map(|view| view.turns)
            .unwrap_or_default();
        if turns.is_empty() {
            return vec![
                div()
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
                    .into_any_element(),
            ];
        }

        let rail_gutter = if show_rail { STUDIO_RAIL_GUTTER } else { 0.0 };
        let available = (f32::from(window.viewport_size().width)
            - crate::settings::SIDEBAR_DEFAULT
            - 240.0
            - 64.0
            - rail_gutter)
            .clamp(240.0, 1600.0);
        let columns = grid_columns(available);
        let gap = if available < 520.0 { 12.0 } else { 16.0 };
        let tile_width = (available - gap * (columns.saturating_sub(1) as f32)) / columns as f32;
        turns
            .iter()
            .enumerate()
            .map(|(turn_ix, turn)| {
                self.render_turn(turn_ix, turn, tile_width, gap, available, theme, cx)
            })
            .collect()
    }

    pub(super) fn render_turn(
        &mut self,
        turn_ix: usize,
        turn: &StudioTurnView,
        tile_width: f32,
        gap: f32,
        content_width: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turn_for_prompt = turn.clone();
        let turn_for_again = turn.clone();
        let turn_for_fork = turn.clone();
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
                                turn_action(
                                    format!("studio-generate-again-{turn_ix}"),
                                    "Generate again",
                                    theme,
                                )
                                .on_click(cx.listener(
                                    move |page, _, _, cx| page.generate_again(&turn_for_again, cx),
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
            .into_any_element()
    }

    /// Left-edge tick rail for the conversation feed — same chrome as the
    /// chat MessageRail, with a Studio-specific hover card.
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
        let tick_rows: Vec<usize> = ticks.iter().map(|tick| tick.turn_ix).collect();
        let top_row = self.feed_scroll.top_item();
        let active = rail::active_tick(&tick_rows, top_row);
        let hover = self.rail_hover;
        let viewport_h = f32::from(self.feed_scroll.bounds().size.height);
        let capacity = rail::rail_slots(if viewport_h > 0.0 { viewport_h } else { 600.0 });
        let buckets = rail::tick_buckets(ticks.len(), capacity);
        let active_bucket = active.and_then(|ix| rail::bucket_of(&buckets, ix));
        let now = Utc::now();

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
            .gap(px(rail::TICK_GAP))
            .children(buckets.into_iter().enumerate().map(|(ix, (start, end))| {
                let rep = active.filter(|&a| a >= start && a < end).unwrap_or(start);
                let tick = ticks[rep].clone();
                let bucket_len = end - start;
                let is_active = active_bucket == Some(ix);
                let is_hovered = hover == Some(ix);
                let bar_width = if is_hovered { 20.0 } else { 12.0 };
                let bar_color = if is_active || is_hovered {
                    theme.text.opacity(0.8)
                } else {
                    crate::theme::ink(0.16)
                };
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
                div()
                    .id(("studio-rail-tick", ix))
                    .relative()
                    .h(px(rail::TICK_SLOT))
                    .w_full()
                    .flex()
                    .items_center()
                    .cursor_pointer()
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        this.rail_hover = if *hovered { Some(ix) } else { None };
                        cx.notify();
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_to_turn(turn_ix, cx);
                    }))
                    .child(
                        div()
                            .h(px(2.0))
                            .w(px(bar_width))
                            .rounded(px(1.0))
                            .bg(bar_color),
                    )
                    .when_some(card, |el, card| {
                        el.child(gpui::deferred(
                            gpui::anchored()
                                .anchor(gpui::Anchor::LeftCenter)
                                .snap_to_window_with_margin(px(8.0))
                                .child(div().pl(px(26.0)).child(card)),
                        ))
                    })
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
