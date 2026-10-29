//! Settings → Archived (feature-inventory §1.5): archived chats across
//! devices, with Unarchive (Mutate setChatArchived false) and a confirmed
//! Delete all (Mutate deleteChat per archived chat). Archived studio threads
//! are listed alongside and can be unarchived; Delete all covers chats only.

use gpui::{
    AnyElement, Context, Entity, SharedString, Subscription, Task, Window, div, prelude::*, px,
};

use zeron_proto::{Chat, StudioConversationSummary};
use zeron_rpc::methods;
use zeron_studio::StudioConversationId;

use crate::popover;
use crate::settings::widgets;
use crate::state::AppState;
use crate::studio::StudioPage;
use crate::theme::Theme;

/// Archived rows in sidebar (recency) order. Pure. Side chats stay listed:
/// once archived and closed, this is the only place to restore them.
pub fn archived_chats(chats: &[Chat]) -> Vec<&Chat> {
    chats.iter().filter(|c| c.archived).collect()
}

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
            // Unknown device → no fragment at all; a Cloud chat's own
            // machine isn't a device worth naming either.
            device: (!zeron_proto::is_cloud_device(&chat.device_id, ""))
                .then(|| device_names.get(&chat.device_id).cloned())
                .flatten(),
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

const ARCHIVE_PAGE_SIZE: usize = 40;

fn archive_page(total: usize, requested: usize) -> usize {
    requested.min(total.saturating_sub(1) / ARCHIVE_PAGE_SIZE)
}

pub struct ArchivedPage {
    state: Entity<AppState>,
    studio: Option<Entity<StudioPage>>,
    scroll: widgets::PageScroll,
    error: Option<SharedString>,
    /// Item with an in-flight unarchive (button shows working state).
    busy: Option<ArchivedId>,
    /// Archived ids captured when Delete all was clicked. The dialog deletes
    /// exactly these, never a chat archived while it was open.
    delete_all_confirm: Option<Vec<String>>,
    /// In-flight Delete all; held so repeat clicks can't start a second run.
    delete_all_task: Option<Task<()>>,
    page: usize,
    task: Option<Task<()>>,
    _observe: Subscription,
    _studio_observe: Option<Subscription>,
}

impl ArchivedPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        Self::build(state, None, cx)
    }

    pub fn with_studio(
        state: Entity<AppState>,
        studio: Entity<StudioPage>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::build(state, Some(studio), cx)
    }

    fn build(
        state: Entity<AppState>,
        studio: Option<Entity<StudioPage>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let studio_observe = studio
            .as_ref()
            .map(|studio| cx.observe(studio, |_, _, cx| cx.notify()));
        Self {
            state,
            studio,
            scroll: widgets::PageScroll::default(),
            error: None,
            busy: None,
            delete_all_confirm: None,
            delete_all_task: None,
            page: 0,
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

    fn request_delete_all(&mut self, cx: &mut Context<Self>) {
        if self.delete_all_task.is_some() {
            return;
        }
        let ids: Vec<String> = archived_chats(&self.state.read(cx).chats)
            .into_iter()
            .map(|chat| chat.id.clone())
            .collect();
        if !ids.is_empty() {
            self.delete_all_confirm = Some(ids);
            cx.notify();
        }
    }

    /// Whether a dialog [`Self::dismiss_on_escape`] would close is open.
    pub(crate) fn has_escape_surface(&self) -> bool {
        self.delete_all_confirm.is_some()
    }

    /// The Delete all dialog, open without archived chats to count.
    #[cfg(test)]
    pub(crate) fn open_delete_all_confirm(&mut self, cx: &mut Context<Self>) {
        self.delete_all_confirm = Some(Vec::new());
        cx.notify();
    }

    /// Escape that reached Settings unclaimed closes the Delete all dialog
    /// first, so it never closes Settings under the dialog (this page
    /// outlives Settings, and the dialog would reopen with it).
    pub(crate) fn dismiss_on_escape(&mut self, cx: &mut Context<Self>) -> bool {
        if self.delete_all_confirm.take().is_none() {
            return false;
        }
        cx.notify();
        true
    }

    fn delete_all(&mut self, cx: &mut Context<Self>) {
        let Some(ids) = self.delete_all_confirm.take() else {
            return;
        };
        if self.delete_all_task.is_some() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            cx.notify();
            return;
        };
        self.error = None;
        // One request per chat, like the sidebar's section archive: the
        // registry tombstones each row, so it leaves this list and the
        // sidebar's Archived shelf as soon as its delete lands.
        self.delete_all_task = Some(cx.spawn(async move |this, cx| {
            let mut failed = 0;
            for chat_id in ids {
                // Unarchived since the confirmation (here, or on another
                // device): no longer part of what the user agreed to delete.
                let still_archived = this
                    .update(cx, |page, cx| {
                        page.state
                            .read(cx)
                            .chats
                            .iter()
                            .any(|chat| chat.id == chat_id && chat.archived)
                    })
                    .unwrap_or(false);
                if !still_archived {
                    continue;
                }
                let params = serde_json::json!({ "op": "deleteChat", "chatId": chat_id });
                if engine.client().call(methods::MUTATE, params).await.is_err() {
                    failed += 1;
                }
            }
            this.update(cx, |page, cx| {
                page.delete_all_task = None;
                if failed > 0 {
                    page.error = Some(delete_all_failure(failed).into());
                }
                cx.notify();
            })
            .ok();
        }));
        cx.notify();
    }

    fn render_delete_all_dialog(
        &mut self,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let ids = self.delete_all_confirm.as_ref()?;
        let count = ids.len();
        let on_cloud = self
            .state
            .read(cx)
            .chats
            .iter()
            .filter(|c| ids.contains(&c.id) && zeron_proto::is_cloud_device(&c.device_id, ""))
            .count();
        let theme = Theme::of(cx).for_popup();
        // Same card as the sidebar's single "Delete session?" confirmation.
        let card = popover::dialog_card(&theme)
            .child(popover::dialog_title(
                &theme,
                "Delete all archived sessions?",
            ))
            .child(
                div()
                    .mt(px(6.0))
                    .child(popover::dialog_body(&theme, delete_all_body(count, on_cloud))),
            )
            .child(
                div()
                    .mt(px(16.0))
                    .flex()
                    .flex_row()
                    .justify_end()
                    .gap(px(8.0))
                    .child(
                        popover::btn_ghost(&theme, "Cancel", "delete-archived-cancel")
                            .id("delete-archived-cancel")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.delete_all_confirm = None;
                                cx.notify();
                            })),
                    )
                    .child(
                        popover::btn_danger(&theme, "Delete all")
                            .id("delete-archived-confirm")
                            .on_click(cx.listener(|this, _, _, cx| this.delete_all(cx))),
                    ),
            )
            .into_any_element();
        Some(popover::modal("delete-archived-dialog", viewport, card))
    }

    fn on_scroll_hovered(&mut self, hovered: &bool, _: &mut Window, cx: &mut Context<Self>) {
        if self.scroll.set_list_hovered(*hovered) {
            cx.notify();
        }
    }
}

impl popover::ScrollRailHost for ArchivedPage {
    fn rail_bar(&mut self) -> &mut popover::MenuScrollbarState {
        self.scroll.rail_bar()
    }

    fn rail_scroll(&self) -> Option<gpui::ScrollHandle> {
        self.scroll.rail_scroll()
    }
}

impl Render for ArchivedPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::toast::take_error(&mut self.error, cx);
        let dialog = self.render_delete_all_dialog(window.viewport_size(), cx);
        let theme = Theme::of(cx).for_settings_surface();
        let now = chrono::Utc::now();
        let (rows, count): (Vec<ArchivedEntry>, usize) = {
            let state = self.state.read(cx);
            let names: std::collections::HashMap<String, String> = state
                .devices
                .iter()
                .map(|d| (d.id.clone(), d.name.clone()))
                .collect();
            let studio_rows = self
                .studio
                .as_ref()
                .map(|studio| studio.read(cx).conversations())
                .unwrap_or(&[]);
            let archived = archived_entries(&state.chats, studio_rows, &names);
            let count = archived.len();
            let page = archive_page(count, self.page);
            if self.page != page {
                self.page = page;
                self.scroll.reset();
            }
            let rows = archived
                .into_iter()
                .skip(self.page * 40)
                .take(40)
                .collect();
            (rows, count)
        };
        let busy = self.busy.clone();
        let page = self.page;
        let items: Vec<AnyElement> = rows
            .into_iter()
            .enumerate()
            .map(|(ix, row)| {
                let title: SharedString = row.title.into();
                let device: Option<SharedString> = row.device.map(Into::into);
                let time_ago: SharedString = crate::state::format_time_ago(row.at, now).into();
                let location: Option<SharedString> = row.location.map(Into::into);
                let is_busy = busy.as_ref() == Some(&row.id);
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
                                            .text_color(theme.text_muted)
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
                                    .text_color(theme.text_muted);
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
                        // Keep the action visible without list-wide hover invalidation.
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
                            .opacity(0.8)
                            .when(is_busy, |el| el.opacity(0.4))
                            .cursor_pointer()
                            .hover(|s| s.bg(theme.surface_raised).text_color(theme.text))
                            .tab_index(0)
                            .role(gpui::Role::Button)
                            .focus_visible(|s| s.border_2().border_color(theme.accent).opacity(1.0))
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
                .text_color(theme.text_muted)
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
                        .text_color(theme.text_muted)
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

        let pagination = (count > ARCHIVE_PAGE_SIZE).then(|| {
            div()
                .mt(px(16.0))
                .flex()
                .items_center()
                .justify_between()
                .child(
                    widgets::ghost_action(&theme)
                        .id("archived-previous")
                        .role(gpui::Role::Button)
                        .aria_label("Previous archived sessions")
                        .tab_index(0)
                        .opacity(if page > 0 { 1.0 } else { 0.4 })
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.page = this.page.saturating_sub(1);
                            this.scroll.reset();
                            cx.notify();
                        }))
                        .child("Previous"),
                )
                .child(format!(
                    "{}–{} of {}",
                    page * ARCHIVE_PAGE_SIZE + 1,
                    ((page + 1) * ARCHIVE_PAGE_SIZE).min(count),
                    count
                ))
                .child(
                    widgets::ghost_action(&theme)
                        .id("archived-next")
                        .role(gpui::Role::Button)
                        .aria_label("Next archived sessions")
                        .tab_index(0)
                        .opacity(if (page + 1) * ARCHIVE_PAGE_SIZE < count {
                            1.0
                        } else {
                            0.4
                        })
                        .focus_visible(|s| s.border_2().border_color(theme.accent))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.page = archive_page(count, this.page + 1);
                            this.scroll.reset();
                            cx.notify();
                        }))
                        .child("Next"),
                )
        });
        let deleting = self.delete_all_task.is_some();
        let delete_all = (count > 0).then(|| {
            widgets::action_button(&theme, widgets::ActionTone::Danger)
                .id("archived-delete-all")
                .flex_none()
                .when(deleting, |el| el.opacity(0.5).cursor_default())
                .tab_index(0)
                .role(gpui::Role::Button)
                .focus_visible(|s| s.border_2().border_color(theme.accent))
                .on_click(cx.listener(|this, _, _, cx| this.request_delete_all(cx)))
                .child(
                    crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                        .size(px(16.0))
                        .text_color(gpui::white()),
                )
                .child(SharedString::from(if deleting {
                    "Deleting…"
                } else {
                    "Delete all"
                }))
        });
        let scrollbar = popover::rail(self, "archived-page-scrollbar", &theme, cx);
        div()
            .id("archived-page-host")
            .relative()
            .size_full()
            .on_hover(cx.listener(Self::on_scroll_hovered))
            .child(
                crate::edge_fade::edge_faded(
                    16.0,
                    true,
                    true,
                    div()
                        .id("archived-page")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll.scroll)
                        .child(
                            widgets::page_column()
                                .child(
                                    // Delete all sits bottom-right, level with
                                    // the subtitle rather than the title.
                                    div()
                                        .flex()
                                        .flex_row()
                                        .items_end()
                                        .gap(px(16.0))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_w_0()
                                                .child(widgets::page_header(
                                                    &theme,
                                                    "Archived sessions",
                                                    (count > 0).then_some(count),
                                                ))
                                                .child(widgets::page_subtitle(
                                                    &theme,
                                                    "Hidden from the sidebar until restored.",
                                                )),
                                        )
                                        .children(delete_all),
                                )
                                .child(body)
                                .children(pagination),
                        ),
                )
                .fade_overflow_y(&self.scroll.scroll),
            )
            .children(scrollbar)
            .children(dialog)
    }
}

fn delete_all_body(count: usize, on_cloud: usize) -> String {
    let sessions = match count {
        1 => "1 archived session".to_string(),
        n => format!("{n} archived sessions"),
    };
    // Cloud chats' machines go with them (the engine deletes each).
    let machines = match on_cloud {
        0 => String::new(),
        1 if count == 1 => " and its Cloud machine".into(),
        1 => ", and 1 Cloud machine,".into(),
        n => format!(", and {n} Cloud machines,"),
    };
    format!("{sessions}{machines} will be permanently deleted. This can\u{2019}t be undone.")
}

fn delete_all_failure(failed: usize) -> String {
    match failed {
        1 => "Couldn\u{2019}t delete 1 archived session. Try again.".into(),
        n => format!("Couldn\u{2019}t delete {n} archived sessions. Try again."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;

    fn chat(id: &str, archived: bool) -> Chat {
        Chat {
            id: id.into(),
            device_id: "d".into(),
            title: None,
            archived,
            cwd: None,
            branch: None,
            checkout_id: None,
            source_context: None,
            config: None,
            last_message_preview: None,
            last_message_at: None,
            created_at: Utc::now(),
            harness_session_id: None,
            harness_session_cwd: None,
            harness_session_harness: None,
            parent_chat_id: None,
            space_id: None,
            last_seen_at: None,
            room_gen: None,
        }
    }

    #[test]
    fn studio_archived_rows_mix_with_chats_by_recency() {
        let mut agent = chat("agent", true);
        agent.title = Some("agent".into());
        agent.last_message_at = Some(Utc::now() - chrono::Duration::minutes(10));
        let studio_rows = vec![
            StudioConversationSummary {
                id: StudioConversationId::new(),
                title: "live".into(),
                turn_count: 1,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            StudioConversationSummary {
                id: StudioConversationId::new(),
                title: "thread".into(),
                turn_count: 1,
                created_at: Utc::now(),
                updated_at: Utc::now() - chrono::Duration::minutes(1),
                archived: true,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
        ];
        let rows = archived_entries(&[agent], &studio_rows, &Default::default());
        let titles: Vec<&str> = rows.iter().map(|row| row.title.as_str()).collect();
        assert_eq!(titles, ["thread", "agent"]);
        assert!(matches!(rows[0].id, ArchivedId::Studio(_)));
        assert_eq!(rows[0].location.as_deref(), Some("Studio"));
    }

    #[test]
    fn archive_page_clamps_after_removal_and_handles_exact_boundaries() {
        assert_eq!(archive_page(0, 9), 0);
        assert_eq!(archive_page(40, 1), 0);
        assert_eq!(archive_page(41, 1), 1);
        assert_eq!(archive_page(80, 2), 1);
        assert_eq!(archive_page(81, 2), 2);
    }

    #[test]
    fn only_archived_rows_show() {
        let chats = vec![chat("a", false), chat("b", true), chat("c", true)];
        let rows = archived_chats(&chats);
        let ids: Vec<&str> = rows.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids, ["b", "c"]);
    }

    #[gpui::test]
    fn delete_all_deletes_the_confirmed_archived_chats_and_reports_failures(
        cx: &mut gpui::TestAppContext,
    ) {
        use gpui::AppContext;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let _guard = runtime.enter();
        let (out, mut requests) = tokio::sync::mpsc::channel(16);
        let (replies, inbound) = tokio::sync::mpsc::channel(16);
        let engine =
            crate::state::EngineHandle::from_test_client(zeron_rpc::RpcClient::new(out, inbound));
        let dir = tempfile::tempdir().unwrap();
        cx.update(|cx| {
            crate::settings::init(Default::default(), dir.path(), cx);
            gpui_base::init(cx);
            cx.set_global(crate::theme::Theme::default());
        });
        let window = cx.add_window(|_, cx| {
            let state = cx.new(|_| {
                let mut state = AppState::new();
                state.chats = vec![
                    chat("live", false),
                    chat("b", true),
                    chat("c", true),
                    chat("restored", true),
                ];
                state.set_test_engine(engine);
                state
            });
            ArchivedPage::new(state, cx)
        });
        window
            .update(cx, |page, _, cx| {
                // Escape closes the dialog before it can close Settings.
                page.request_delete_all(cx);
                assert!(page.dismiss_on_escape(cx));
                assert!(page.delete_all_confirm.is_none());
                assert!(!page.dismiss_on_escape(cx));
                page.request_delete_all(cx);
                assert_eq!(
                    page.delete_all_confirm.as_deref(),
                    Some(&["b".to_string(), "c".to_string(), "restored".to_string()][..])
                );
                // Archived while the dialog was open: not part of what the
                // user confirmed, so it must survive.
                page.state
                    .update(cx, |state, _| state.chats.push(chat("late", true)));
                page.delete_all(cx);
                assert!(page.delete_all_confirm.is_none());
            })
            .unwrap();
        for (index, expected) in ["b", "c"].into_iter().enumerate() {
            cx.run_until_parked();
            let request: serde_json::Value = serde_json::from_str(
                &requests
                    .try_recv()
                    .expect("Each confirmed archived chat must be deleted"),
            )
            .unwrap();
            assert_eq!(request["params"]["op"], "deleteChat");
            assert_eq!(request["params"]["chatId"], expected);
            if index == 0 {
                // Unarchived mid-run (e.g. on another device): spared.
                window
                    .update(cx, |page, _, cx| {
                        page.state.update(cx, |state, _| {
                            state
                                .chats
                                .iter_mut()
                                .filter(|chat| chat.id == "restored")
                                .for_each(|chat| chat.archived = false)
                        })
                    })
                    .unwrap();
            }
            let reply = if index == 0 {
                serde_json::json!({"id":request["id"],"err":"rejected"})
            } else {
                serde_json::json!({"id":request["id"],"ok":{"ok":true}})
            };
            runtime.block_on(async {
                replies.send(reply.to_string()).await.unwrap();
                while replies.capacity() < replies.max_capacity() {
                    tokio::task::yield_now().await;
                }
            });
        }
        cx.run_until_parked();
        assert!(
            requests.try_recv().is_err(),
            "Only the confirmed chats are deleted"
        );
        window
            .update(cx, |page, _, cx| {
                assert!(page.delete_all_task.is_none());
                let notice = page
                    .error
                    .clone()
                    .or_else(|| crate::toast::messages(cx).last().cloned());
                assert_eq!(
                    notice.as_deref(),
                    Some("Couldn\u{2019}t delete 1 archived session. Try again.")
                );
            })
            .unwrap();
    }

    #[test]
    fn delete_all_copy_handles_one_and_many() {
        assert!(delete_all_body(1, 0).starts_with("1 archived session will"));
        assert!(delete_all_body(146, 0).starts_with("146 archived sessions will"));
        assert!(delete_all_body(1, 1).starts_with("1 archived session and its Cloud machine will"));
        assert!(
            delete_all_body(3, 1).starts_with("3 archived sessions, and 1 Cloud machine, will")
        );
        assert!(
            delete_all_body(3, 2).starts_with("3 archived sessions, and 2 Cloud machines, will")
        );
    }

    #[test]
    fn archived_side_chats_stay_restorable() {
        let mut side = chat("side", true);
        side.parent_chat_id = Some("main".into());
        let chats = vec![chat("main", false), side];
        let ids: Vec<&str> = archived_chats(&chats)
            .iter()
            .map(|c| c.id.as_str())
            .collect();
        assert_eq!(ids, ["side"]);
    }
}
