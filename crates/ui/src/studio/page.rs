//! Studio conversation page: load, submit, and the feed/lightbox outlet.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use gpui::{
    Context, Entity, EventEmitter, FocusHandle, Focusable, Image, Pixels, Point, Render,
    SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::{
    ListStudioConversationsResponse, ListStudioModelsResponse, ListStudioProvidersResponse,
    StudioConversationSummary, StudioConversationView, StudioProviderConnection, StudioTurnView,
};
use zeron_rpc::methods;
use zeron_studio::{StudioArtifactId, StudioConversationId};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

use super::StudioEvent;
use super::artifact::read_artifact_image;
use super::draft::{DraftRunConfig, draft_aspect};
use super::feed::{STUDIO_COMPOSER_CLEARANCE, STUDIO_RAIL_GUTTER};

pub struct StudioPage {
    pub(super) state: Entity<AppState>,
    pub(super) conversations: Vec<StudioConversationSummary>,
    pub(super) providers: Vec<StudioProviderConnection>,
    pub(super) models: Vec<zeron_studio::MediaModel>,
    pub(super) selected_models: BTreeSet<zeron_studio::ModelId>,
    pub(super) draft_runs: HashMap<zeron_studio::ModelId, DraftRunConfig>,
    pub(super) selected_conversation: Option<StudioConversationId>,
    pub(super) conversation: Option<StudioConversationView>,
    pub(super) prompt: Entity<ComposerInput>,
    pub(super) model_search: Entity<ComposerInput>,
    pub(super) model_picker: popover::Popup<()>,
    pub(super) model_picker_active: Option<usize>,
    pub(super) model_picker_scroll: gpui::ScrollHandle,
    pub(super) model_picker_focus: FocusHandle,
    pub(super) feed_scroll: gpui::ScrollHandle,
    pub(super) artifact_filmstrip_scroll: gpui::ScrollHandle,
    pub(super) scroll_after_turn_count: Option<usize>,
    pub(super) scroll_task: Option<Task<()>>,
    pub(super) rail_hover: Option<usize>,
    pub(super) source_turn: Option<zeron_studio::StudioTurnId>,
    pub(super) images: HashMap<StudioArtifactId, Arc<Image>>,
    pub(super) loading_images: HashSet<StudioArtifactId>,
    pub(super) selected_artifact: Option<StudioArtifactId>,
    pub(super) lightbox_zoom: f32,
    pub(super) lightbox_pan: Point<Pixels>,
    pub(super) lightbox_drag: Option<Point<Pixels>>,
    pub(super) lightbox_swipe_x: f32,
    pub(super) lightbox_swipe_velocity: f32,
    pub(super) lightbox_snap: Option<super::artifact::LightboxSnap>,
    pub(super) lightbox_swipe_scheduled: bool,
    pub(super) lightbox_swipe_last_tick: Option<Instant>,
    pub(super) lightbox_ignore_scroll_until: Option<Instant>,
    pub(super) lightbox_stage_width: f32,
    pub(super) filmstrip_scroll_accum: f32,
    pub(super) focus: FocusHandle,
    pub(super) loading: bool,
    pub(super) busy: bool,
    pub(super) error: Option<SharedString>,
    pub(super) load_task: Option<Task<()>>,
    pub(super) watch_task: Option<Task<()>>,
    pub(super) action_task: Option<Task<()>>,
    pub(super) image_tasks: HashMap<StudioArtifactId, Task<()>>,
    pub(super) _observe: Subscription,
    pub(super) _prompt_events: Subscription,
    pub(super) _model_search_events: Subscription,
}

impl StudioPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let prompt = cx.new(|cx| ComposerInput::new("Describe the image you want to create", cx));
        let model_search = cx.new(|cx| ComposerInput::palette_search("Search models…", cx));
        let prompt_events = cx.subscribe(&prompt, |page: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => page.submit(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let model_search_events =
            cx.subscribe(&model_search, |page: &mut Self, _, event, cx| match event {
                ComposerInputEvent::Edited => {
                    page.model_picker_active = None;
                    page.model_picker_scroll.set_offset(Point::default());
                    cx.notify();
                }
                ComposerInputEvent::Submitted => page.activate_model_picker_row(cx),
                _ => {}
            });
        let mut page = Self {
            state,
            conversations: Vec::new(),
            providers: Vec::new(),
            models: Vec::new(),
            selected_models: BTreeSet::new(),
            draft_runs: HashMap::new(),
            selected_conversation: None,
            conversation: None,
            prompt,
            model_search,
            model_picker: popover::Popup::default(),
            model_picker_active: None,
            model_picker_scroll: gpui::ScrollHandle::new(),
            model_picker_focus: cx.focus_handle(),
            feed_scroll: gpui::ScrollHandle::new(),
            artifact_filmstrip_scroll: gpui::ScrollHandle::new(),
            scroll_after_turn_count: None,
            scroll_task: None,
            rail_hover: None,
            source_turn: None,
            images: HashMap::new(),
            loading_images: HashSet::new(),
            selected_artifact: None,
            lightbox_zoom: 1.0,
            lightbox_pan: Point::default(),
            lightbox_drag: None,
            lightbox_swipe_x: 0.0,
            lightbox_swipe_velocity: 0.0,
            lightbox_snap: None,
            lightbox_swipe_scheduled: false,
            lightbox_swipe_last_tick: None,
            lightbox_ignore_scroll_until: None,
            lightbox_stage_width: 0.0,
            filmstrip_scroll_accum: 0.0,
            focus: cx.focus_handle(),
            loading: false,
            busy: false,
            error: None,
            load_task: None,
            watch_task: None,
            action_task: None,
            image_tasks: HashMap::new(),
            _observe: observe,
            _prompt_events: prompt_events,
            _model_search_events: model_search_events,
        };
        page.load(cx);
        page
    }

    pub(super) fn engine(&self, cx: &Context<Self>) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    pub fn load(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let Some(engine) = self.engine(cx) else {
            self.error = Some("Engine not connected".into());
            return;
        };
        self.loading = true;
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let providers = engine
                .client()
                .call(methods::LIST_STUDIO_PROVIDERS, serde_json::json!({}))
                .await;
            let conversations = engine
                .client()
                .call(methods::LIST_STUDIO_CONVERSATIONS, serde_json::json!({}))
                .await;
            let mut models = Ok(ListStudioModelsResponse {
                models: Vec::new(),
                fetched_at: chrono::Utc::now(),
                stale: false,
            });
            if let Ok(value) = &providers
                && let Ok(list) = serde_json::from_value::<ListStudioProvidersResponse>(value.clone())
                && let Some(provider) = list.providers.iter().find(|provider| provider.configured)
            {
                models = engine
                    .client()
                    .call(
                        methods::LIST_STUDIO_MODELS,
                        serde_json::json!({ "providerId": provider.provider_id, "mediaKind": "image" }),
                    )
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|value| serde_json::from_value(value).map_err(|error| error.to_string()));
            }
            this.update(cx, |page, cx| {
                page.loading = false;
                match (providers, conversations, models) {
                    (Ok(providers), Ok(conversations), Ok(models)) => {
                        page.providers = serde_json::from_value::<ListStudioProvidersResponse>(providers)
                            .map(|value| value.providers)
                            .unwrap_or_default();
                        page.conversations = serde_json::from_value::<ListStudioConversationsResponse>(conversations)
                            .map(|value| value.conversations)
                            .unwrap_or_default();
                        page.apply_models(models.models);
                        if page.selected_models.is_empty()
                            && let Some(model) = page.models.first()
                        {
                            page.selected_models.insert(model.id.clone());
                        }
                        if page.selected_conversation.is_none()
                            && let Some(conversation) = page.conversations.first()
                        {
                            page.open_conversation(conversation.id, cx);
                        }
                        cx.emit(StudioEvent::SidebarChanged);
                    }
                    (Err(error), _, _) | (_, Err(error), _) => page.error = Some(error.to_string().into()),
                    (_, _, Err(error)) => page.error = Some(error.into()),
                }
                cx.notify();
            }).ok();
        }));
    }

    pub fn conversations(&self) -> &[StudioConversationSummary] {
        &self.conversations
    }

    pub fn selected_conversation(&self) -> Option<StudioConversationId> {
        self.selected_conversation
    }

    pub fn open_conversation(&mut self, id: StudioConversationId, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if self.selected_conversation != Some(id) {
            self.close_artifact(cx);
        }
        self.selected_conversation = Some(id);
        self.conversation = None;
        self.scroll_after_turn_count = None;
        self.scroll_task = None;
        self.rail_hover = None;
        self.feed_scroll.set_offset(Point::default());
        cx.emit(StudioEvent::SidebarChanged);
        self.watch_task = Some(cx.spawn(async move |this, cx| {
            let stream = engine
                .client()
                .subscribe(
                    methods::WATCH_STUDIO_CONVERSATION,
                    serde_json::json!({ "conversationId": id }),
                )
                .await;
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            while let Some(value) = stream.recv().await {
                let Ok(view) = serde_json::from_value::<StudioConversationView>(value) else {
                    continue;
                };
                if this
                    .update(cx, |page, cx| {
                        let submitted_turn_arrived = page
                            .scroll_after_turn_count
                            .is_some_and(|before| view.turns.len() > before);
                        page.conversation = Some(view);
                        if submitted_turn_arrived {
                            page.scroll_after_turn_count = None;
                            page.feed_scroll.scroll_to_bottom();
                        }
                        page.start_missing_image_loads(cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    pub(super) fn start_missing_image_loads(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let ids = self
            .conversation
            .iter()
            .flat_map(|view| &view.turns)
            .flat_map(|turn| &turn.runs)
            .flat_map(|run| &run.artifacts)
            .map(|artifact| artifact.id)
            .filter(|id| !self.images.contains_key(id) && !self.loading_images.contains(id))
            .collect::<Vec<_>>();
        for id in ids {
            self.loading_images.insert(id);
            let engine = engine.clone();
            let task = cx.spawn(async move |this, cx| {
                let image = read_artifact_image(&engine, id).await;
                this.update(cx, |page, cx| {
                    page.loading_images.remove(&id);
                    if let Ok(image) = image {
                        page.images.insert(id, image);
                    }
                    page.image_tasks.remove(&id);
                    cx.notify();
                })
                .ok();
            });
            self.image_tasks.insert(id, task);
        }
    }

    pub fn new_conversation(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CREATE_STUDIO_CONVERSATION,
                    serde_json::json!({ "title": "Untitled study" }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result.and_then(|value| {
                    serde_json::from_value::<StudioConversationSummary>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(conversation) => {
                        page.conversations.insert(0, conversation.clone());
                        page.open_conversation(conversation.id, cx);
                        cx.emit(StudioEvent::SidebarChanged);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let Some(conversation_id) = self.selected_conversation else {
            self.error = Some("Create a conversation first".into());
            return;
        };
        let prompt = self.prompt.read(cx).text().trim().to_owned();
        if prompt.is_empty() || self.selected_models.is_empty() || self.busy {
            return;
        }
        let runs = self
            .models
            .iter()
            .filter(|model| self.selected_models.contains(&model.id))
            .map(|model| {
                let draft = self
                    .draft_runs
                    .get(&model.id)
                    .cloned()
                    .unwrap_or_else(|| DraftRunConfig::from_model(model));
                let display_aspect_ratio = draft_aspect(model, &draft);
                serde_json::json!({
                    "providerId": model.provider_id, "modelId": model.id,
                    "operation": model.operation, "outputCount": draft.output_count, "controls": draft.controls,
                    "inputs": [], "manifestVersion": model.manifest_version,
                    "displayAspectRatio": display_aspect_ratio,
                })
            })
            .collect::<Vec<_>>();
        let source = self.source_turn;
        let provider_id = self
            .providers
            .iter()
            .find(|provider| provider.configured)
            .map(|provider| provider.provider_id.clone());
        self.scroll_after_turn_count = Some(
            self.conversation
                .as_ref()
                .map_or(0, |view| view.turns.len()),
        );
        self.feed_scroll.scroll_to_bottom();
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CREATE_STUDIO_TURN,
                    serde_json::json!({
                        "conversationId": conversation_id, "prompt": prompt, "runs": runs,
                        "sourceTurnId": source,
                    }),
                )
                .await;
            let models = if let Some(provider_id) = provider_id {
                engine
                    .client()
                    .call(
                        methods::LIST_STUDIO_MODELS,
                        serde_json::json!({ "providerId": provider_id, "mediaKind": "image" }),
                    )
                    .await
                    .ok()
                    .and_then(|value| {
                        serde_json::from_value::<ListStudioModelsResponse>(value).ok()
                    })
            } else {
                None
            };
            this.update(cx, |page, cx| {
                page.busy = false;
                if let Some(models) = models {
                    page.apply_models(models.models);
                }
                match result {
                    Ok(_) => {
                        page.prompt.update(cx, |input, cx| input.set_text("", cx));
                        page.source_turn = None;
                    }
                    Err(error) => {
                        page.scroll_after_turn_count = None;
                        page.error = Some(error.to_string().into());
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn apply_models(&mut self, models: Vec<zeron_studio::MediaModel>) {
        self.models = models;
        for model in &self.models {
            self.draft_runs
                .entry(model.id.clone())
                .or_insert_with(|| DraftRunConfig::from_model(model));
        }
    }

    pub(super) fn use_prompt(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
        self.prompt
            .update(cx, |input, cx| input.set_text(turn.prompt.clone(), cx));
        self.selected_models = turn.runs.iter().map(|run| run.model.id.clone()).collect();
        for run in &turn.runs {
            self.draft_runs.insert(
                run.model.id.clone(),
                DraftRunConfig {
                    output_count: run.output_count,
                    controls: run.controls.clone(),
                },
            );
        }
        self.source_turn = Some(turn.id);
        cx.notify();
    }

    pub(super) fn generate_again(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
        self.use_prompt(turn, cx);
        self.submit(cx);
    }

    pub(super) fn fork_from(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let snapshot = turn.clone();
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CREATE_STUDIO_CONVERSATION,
                    serde_json::json!({
                        "title": format!("Fork: {}", snapshot.prompt.chars().take(48).collect::<String>()),
                        "forkedFromTurnId": snapshot.id,
                    }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                match result.and_then(|value| {
                    serde_json::from_value::<StudioConversationSummary>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(conversation) => {
                        page.conversations.insert(0, conversation.clone());
                        page.open_conversation(conversation.id, cx);
                        page.use_prompt(&snapshot, cx);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn archive_conversation(
        &mut self,
        conversation_id: StudioConversationId,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::ARCHIVE_STUDIO_CONVERSATION,
                    serde_json::json!({ "conversationId": conversation_id, "archived": true }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.conversations.retain(|item| item.id != conversation_id);
                        if page.selected_conversation == Some(conversation_id) {
                            page.selected_conversation = None;
                            page.conversation = None;
                            if let Some(next) = page.conversations.first() {
                                page.open_conversation(next.id, cx);
                            }
                        }
                        cx.emit(StudioEvent::SidebarChanged);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn retry(
        &mut self,
        run_id: zeron_studio::StudioRunId,
        retry_anyway: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RETRY_STUDIO_RUN,
                    serde_json::json!({ "runId": run_id, "retryAnyway": retry_anyway }),
                )
                .await;
            this.update(cx, |page, cx| {
                if let Err(error) = result {
                    page.error = Some(error.to_string().into());
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

impl EventEmitter<StudioEvent> for StudioPage {}
impl Focusable for StudioPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for StudioPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.lightbox_snap.is_some() && crate::motion::reduced_motion(cx) {
            self.finish_lightbox_snap_immediate(cx);
        } else if self.lightbox_snap.is_some() && !self.lightbox_swipe_scheduled {
            self.lightbox_swipe_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |page: &mut StudioPage, cx| {
                        page.lightbox_swipe_scheduled = false;
                        page.step_lightbox_snap(cx);
                    })
                    .ok();
            });
        }
        let theme = Theme::of(cx).clone();
        let body = if let Some(page) = self.render_artifact_page(&theme, cx) {
            page
        } else if self.providers.iter().all(|provider| !provider.configured) {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .font_weight(gpui::FontWeight::SEMIBOLD)
                        .child("Connect a provider to begin"),
                )
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.text_muted)
                        .child("Connect an image provider to start generating."),
                )
                .child(
                    div()
                        .id("studio-add-provider")
                        .mt(px(16.0))
                        .w(px(240.0))
                        .h(px(44.0))
                        .cursor_pointer()
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.border)
                        .bg(crate::theme::ink(0.02))
                        .text_color(theme.text)
                        .px(px(14.0))
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .font_weight(gpui::FontWeight::MEDIUM)
                        .hover(|style| {
                            style
                                .bg(crate::theme::ink(0.05))
                                .border_color(theme.border_strong)
                        })
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(StudioEvent::OpenProviders);
                        }))
                        .child(
                            crate::icons::icon(crate::icons::KEY_MINIMALISTIC)
                                .size(px(15.0))
                                .text_color(theme.text_muted),
                        )
                        .child("Add provider"),
                )
                .into_any_element()
        } else {
            let has_turns = self
                .conversation
                .as_ref()
                .is_some_and(|view| !view.turns.is_empty());
            let show_rail = has_turns && self.rail_should_show(self.feed_container_width(window));
            let left_pad = if show_rail {
                24.0 + STUDIO_RAIL_GUTTER
            } else {
                24.0
            };
            let rail = self.render_studio_rail(window, &theme, cx);
            div()
                .relative()
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_hidden()
                .child(
                    div()
                        .id("studio-feed-scroll")
                        .size_full()
                        .overflow_y_scroll()
                        .track_scroll(&self.feed_scroll)
                        .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                        .when(has_turns, |feed| {
                            feed.pt(px(22.0))
                                .pl(px(left_pad))
                                .pr(px(24.0))
                                .pb(px(STUDIO_COMPOSER_CLEARANCE))
                                .flex()
                                .flex_col()
                                .gap(px(28.0))
                        })
                        .children(self.render_feed(window, &theme, show_rail, cx)),
                )
                .child(rail)
                .child(self.render_composer(&theme, cx))
                .into_any_element()
        };
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .child(body)
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .absolute()
                        .top(px(12.0))
                        .right(px(12.0))
                        .rounded(px(8.0))
                        .bg(theme.danger)
                        .text_color(theme.bg)
                        .px(px(10.0))
                        .py(px(6.0))
                        .child(error),
                )
            })
    }
}
