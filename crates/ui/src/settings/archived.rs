//! Settings → Archived (feature-inventory §1.5): archived chats and studio
//! threads, with Unarchive.

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use zeron_proto::{Chat, StudioConversationSummary};
use zeron_rpc::methods;
use zeron_studio::StudioConversationId;

use crate::state::AppState;
use crate::studio::StudioPage;
use crate::theme::Theme;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArchivedId {
    Chat(String),
    Studio(StudioConversationId),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ArchivedEntry {
    pub id: ArchivedId,
    pub title: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub device: Option<String>,
    pub location: Option<String>,
}

/// Archived chats in sidebar (recency) order. Pure.
pub fn archived_chats(chats: &[Chat]) -> Vec<&Chat> {
    chats.iter().filter(|c| c.archived).collect()
}

pub fn archived_studio(
    conversations: &[StudioConversationSummary],
) -> Vec<&StudioConversationSummary> {
    conversations.iter().filter(|c| c.archived).collect()
}

/// Combined archived list: agent sessions and studio threads, newest first.
pub fn archived_entries(
    chats: &[Chat],
    studio: &[StudioConversationSummary],
    device_names: &std::collections::HashMap<String, String>,
) -> Vec<ArchivedEntry> {
    let mut rows: Vec<ArchivedEntry> = archived_chats(chats)
        .into_iter()
        .map(|chat| ArchivedEntry {
            id: ArchivedId::Chat(chat.id.clone()),
            title: chat
                .title
                .clone()
                .unwrap_or_else(|| "Untitled session".into()),
            at: chat.last_message_at.unwrap_or(chat.created_at),
            device: device_names.get(&chat.device_id).cloned(),
            location: crate::state::chat_location(chat),
        })
        .collect();
    rows.extend(
        archived_studio(studio)
            .into_iter()
            .map(|conversation| ArchivedEntry {
                id: ArchivedId::Studio(conversation.id),
                title: conversation.title.clone(),
                at: conversation.updated_at,
                device: None,
                location: Some("Studio".into()),
            }),
    );
    rows.sort_by(|left, right| {
        right
            .at
            .cmp(&left.at)
            .then_with(|| match (&left.id, &right.id) {
                (ArchivedId::Chat(a), ArchivedId::Chat(b)) => a.cmp(b),
                (ArchivedId::Studio(a), ArchivedId::Studio(b)) => a.0.cmp(&b.0),
                (ArchivedId::Chat(_), ArchivedId::Studio(_)) => std::cmp::Ordering::Less,
                (ArchivedId::Studio(_), ArchivedId::Chat(_)) => std::cmp::Ordering::Greater,
            })
    });
    rows
}

pub struct ArchivedPage {
    state: Entity<AppState>,
    studio: Entity<StudioPage>,
    error: Option<SharedString>,
    /// Item with an in-flight unarchive (button shows working state).
    busy: Option<ArchivedId>,
    /// Row index under the pointer — drives the original's `group-hover`
    /// Unarchive reveal (`opacity-0 group-hover:opacity-100`).
    hovered: Option<usize>,
    task: Option<Task<()>>,
    _observe: Subscription,
    _studio_observe: Subscription,
}

impl ArchivedPage {
    pub fn new(
        state: Entity<AppState>,
        studio: Entity<StudioPage>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let studio_observe = cx.observe(&studio, |_, _, cx| cx.notify());
        Self {
            state,
            studio,
            error: None,
            busy: None,
            hovered: None,
            task: None,
            _observe: observe,
            _studio_observe: studio_observe,
        }
    }

    fn unarchive(&mut self, id: ArchivedId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.busy = Some(id.clone());
        self.error = None;
        let params = match &id {
            ArchivedId::Chat(chat_id) => serde_json::json!({
                "op": "setChatArchived",
                "chatId": chat_id,
                "archived": false,
            }),
            ArchivedId::Studio(conversation_id) => serde_json::json!({
                "conversationId": conversation_id,
                "archived": false,
            }),
        };
        let method = match &id {
            ArchivedId::Chat(_) => methods::MUTATE,
            ArchivedId::Studio(_) => methods::ARCHIVE_STUDIO_CONVERSATION,
        };
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine.client().call(method, params).await;
            this.update(cx, |page, cx| {
                page.busy = None;
                if let Err(err) = result {
                    page.error = Some(format!("Unarchive failed: {err}").into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }
}

impl Render for ArchivedPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        use crate::settings::widgets;
        let theme = Theme::of(cx).clone();
        let now = chrono::Utc::now();
        let rows = {
            let state = self.state.read(cx);
            let names = state
                .devices
                .iter()
                .map(|d| (d.id.clone(), d.name.clone()))
                .collect();
            archived_entries(&state.chats, self.studio.read(cx).conversations(), &names)
        };
        let busy = self.busy.clone();
        let count = rows.len();

        let items: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let title: SharedString = row.title.into();
                let device: Option<SharedString> = row.device.map(Into::into);
                let time_ago: SharedString = crate::state::format_time_ago(row.at, now).into();
                let location: Option<SharedString> = row.location.map(Into::into);
                let is_busy = busy.as_ref() == Some(&row.id);
                let row_hovered = self.hovered == Some(ix);
                let item_id = row.id.clone();
                // zeron settings.archived.tsx row: archive tile, medium title
                // + tabular time, quiet device · location meta, Unarchive.
                div()
                    .id(("archived-row", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap(px(12.0))
                    .rounded(px(8.0))
                    .px(px(12.0))
                    .py(px(8.0))
                    .hover(|s| s.bg(crate::theme::ink(0.03)))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered {
                            this.hovered = Some(ix);
                        } else if this.hovered == Some(ix) {
                            this.hovered = None;
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(32.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                crate::icons::icon(crate::icons::ARCHIVE_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted.opacity(0.6)),
                            ),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .text_size(crate::typography::ui_rems(13.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .flex_none()
                                            .text_size(crate::typography::ui_rems(11.0))
                                            .text_color(theme.text_muted.opacity(0.5))
                                            .child(time_ago),
                                    ),
                            )
                            .child({
                                // device · location, separator at the line's
                                // own tone (zeron: a plain span inheriting
                                // `text-muted-foreground/55`).
                                let mut meta = div()
                                    .mt(px(2.0))
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap(px(6.0))
                                    .text_size(crate::typography::ui_rems(11.0))
                                    .text_color(theme.text_muted.opacity(0.55));
                                let both = device.is_some() && location.is_some();
                                if let Some(device) = device {
                                    meta = meta.child(device);
                                }
                                if both {
                                    meta = meta.child(SharedString::from("·"));
                                }
                                if let Some(location) = location {
                                    meta = meta.child(div().min_w_0().truncate().child(location));
                                }
                                meta
                            }),
                    )
                    .child(
                        // Hidden until the row is hovered (zeron `opacity-0
                        // group-hover:opacity-100`); hover fill is the solid
                        // accent tone (`hover:bg-accent`).
                        div()
                            .id(("unarchive", ix))
                            .flex_none()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap(px(6.0))
                            .px(px(10.0))
                            .py(px(4.0))
                            .rounded(px(6.0))
                            .border_1()
                            .border_color(theme.border)
                            .text_size(crate::typography::ui_rems(12.0))
                            .text_color(theme.text_muted)
                            .opacity(if row_hovered || is_busy { 1.0 } else { 0.0 })
                            .when(is_busy, |el| el.opacity(0.4))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.surface_raised).text_color(theme.text))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.unarchive(item_id.clone(), cx);
                            }))
                            .child(
                                crate::icons::icon(crate::icons::ARCHIVE_UP_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child(SharedString::from(if is_busy {
                                "Unarchiving…"
                            } else {
                                "Unarchive"
                            })),
                    )
                    .into_any_element()
            })
            .collect();

        let body: AnyElement = if items.is_empty() {
            // Centered empty state (zeron settings.archived.tsx).
            div()
                .mt(px(96.0))
                .flex()
                .flex_col()
                .items_center()
                .text_center()
                .text_color(theme.text_muted.opacity(0.5))
                .child(
                    // `opacity-40` on top of the inherited muted/50 — an
                    // effectively ~20% glyph (zeron settings.archived.tsx).
                    crate::icons::icon(crate::icons::ARCHIVE_MINIMALISTIC)
                        .size(px(28.0))
                        .text_color(theme.text_muted.opacity(0.2)),
                )
                .child(
                    div()
                        .mt(px(12.0))
                        .text_size(crate::typography::ui_rems(14.0))
                        .child(SharedString::from("Nothing archived")),
                )
                .child(
                    div()
                        .mt(px(4.0))
                        .text_size(crate::typography::ui_rems(12.0))
                        .text_color(theme.text_muted.opacity(0.4))
                        .child(SharedString::from(
                            "Right-click a session in the sidebar to archive it.",
                        )),
                )
                .into_any_element()
        } else {
            div()
                .mt(px(24.0))
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(items)
                .into_any_element()
        };

        div()
            .id("archived-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(
                        &theme,
                        "Archived sessions",
                        (count > 0).then_some(count),
                    ))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Hidden from the sidebar, never deleted. Unarchive to put a session back.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("archived-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .child(body),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn chat(id: &str, archived: bool, minutes_ago: i64) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: Some(id.into()),
            archived,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: Some(Utc::now() - Duration::minutes(minutes_ago)),
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    fn studio(title: &str, archived: bool, minutes_ago: i64) -> StudioConversationSummary {
        StudioConversationSummary {
            id: StudioConversationId::new(),
            title: title.into(),
            turn_count: 1,
            created_at: Utc::now(),
            updated_at: Utc::now() - Duration::minutes(minutes_ago),
            archived,
            forked_from_turn_id: None,
            creating: false,
            done: false,
        }
    }

    #[test]
    fn only_archived_rows_show() {
        let chats = vec![chat("a", false, 0), chat("b", true, 0), chat("c", true, 0)];
        let rows = archived_chats(&chats);
        let ids: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[test]
    fn studio_archived_rows_mix_with_chats_by_recency() {
        let chats = vec![chat("agent", true, 10)];
        let studio_rows = vec![studio("live", false, 0), studio("thread", true, 1)];
        let names = std::collections::HashMap::new();
        let rows = archived_entries(&chats, &studio_rows, &names);
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["thread", "agent"]);
        assert!(matches!(rows[0].id, ArchivedId::Studio(_)));
        assert_eq!(rows[0].location.as_deref(), Some("Studio"));
        assert!(matches!(rows[1].id, ArchivedId::Chat(_)));
    }
}
