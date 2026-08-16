//! First-release Studio viewport and provider settings.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine as _;
use chrono::{DateTime, TimeZone, Utc};
use gpui::{
    AnyElement, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable, Image,
    ImageFormat, KeyDownEvent, ObjectFit, PinchEvent, Pixels, Point, Render, ScrollWheelEvent,
    SharedString, Subscription, Task, TouchPhase, Window, div, img, prelude::*, px,
};
use zeron_proto::{
    ListStudioConversationsResponse, ListStudioModelsResponse, ListStudioProvidersResponse,
    ProviderValidationState, StudioConversationSummary, StudioConversationView,
    StudioProviderConnection, StudioRunState, StudioTurnView,
};
use zeron_rpc::methods;
use zeron_studio::{StudioArtifactId, StudioConversationId};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::icons;
use crate::motion;
use crate::popover;
use crate::rail;
use crate::settings::widgets;
use crate::state::{AppState, EngineHandle, format_time_ago};
use crate::theme::Theme;
use crate::transcript::format_timestamp;

/// Scroll runway below the final Studio turn. The composer floats 18px above
/// the viewport and is 191px tall at its largest first-release configuration;
/// the remaining space keeps the last image clear of the glass card.
const STUDIO_COMPOSER_CLEARANCE: f32 = 256.0;
const ARTIFACT_SWIPE_COMMIT: f32 = 64.0;
const ARTIFACT_SWIPE_LIMIT: f32 = 180.0;
const ARTIFACT_FILMSTRIP_STEP: f32 = 38.0;
/// Extra left inset so the tick rail (16px + 20px hover bar) does not cover
/// the prompt header. Matches the chat transcript's wide-gutter band.
const STUDIO_RAIL_GUTTER: f32 = 28.0;

/// One feed-rail tick: a Studio turn's prompt, the models that ran it, and
/// when it was sent.
#[derive(Debug, Clone, PartialEq)]
struct StudioRailTick {
    turn_ix: usize,
    prompt: String,
    models: Vec<(String, u32)>,
    created_at: DateTime<Utc>,
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

fn stepped_artifact_index(index: usize, len: usize, delta: isize, wraps: bool) -> usize {
    if len == 0 {
        return 0;
    }
    if wraps {
        (index as isize + delta).rem_euclid(len as isize) as usize
    } else {
        (index as isize + delta).clamp(0, len.saturating_sub(1) as isize) as usize
    }
}

fn step_artifact_swipe_spring(mut position: f32, mut velocity: f32, mut frames: f32) -> (f32, f32) {
    while frames > 0.0 {
        let step = frames.min(1.0);
        frames -= step;
        velocity += -position * 0.18 * step;
        velocity *= 0.76_f32.powf(step);
        position += velocity * step;
    }
    (position, velocity)
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

fn default_aspect(model: &zeron_studio::MediaModel) -> (u32, u32) {
    model
        .controls
        .iter()
        .find(|control| control.id.as_str() == "aspect_ratio")
        .and_then(|control| control.default.as_ref())
        .and_then(|value| match value {
            zeron_studio::ControlValue::AspectRatio { width, height } => Some((*width, *height)),
            _ => None,
        })
        .unwrap_or((1, 1))
}

fn draft_aspect(model: &zeron_studio::MediaModel, draft: &DraftRunConfig) -> (u32, u32) {
    draft
        .controls
        .values()
        .find_map(|value| match value {
            zeron_studio::ControlValue::AspectRatio { width, height } => Some((*width, *height)),
            zeron_studio::ControlValue::Dimensions { width, height } => Some((*width, *height)),
            _ => None,
        })
        .unwrap_or_else(|| default_aspect(model))
}

fn write_artifact_file(destination: PathBuf, bytes: Vec<u8>) -> Result<(), String> {
    std::fs::write(destination, bytes).map_err(|error| error.to_string())
}

fn boolean_control_chip(
    id: impl Into<SharedString>,
    label: &'static str,
    on: bool,
    theme: &Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id.into())
        .h(px(27.0))
        .px(px(7.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .rounded(px(7.0))
        .bg(crate::theme::wash(if on { 0.12 } else { 0.07 }))
        .cursor_pointer()
        .child(
            div()
                .w(px(18.0))
                .h(px(10.0))
                .rounded_full()
                .bg(if on {
                    theme.text_muted
                } else {
                    theme.border_strong
                })
                .p(px(2.0))
                .child(
                    div()
                        .size(px(6.0))
                        .rounded_full()
                        .bg(theme.surface)
                        .when(on, |dot| dot.ml(px(8.0))),
                ),
        )
        .child(
            div()
                .text_size(px(10.5))
                .text_color(theme.text_muted)
                .child(label),
        )
}

fn control_value_label(value: &zeron_studio::ControlValue) -> String {
    use zeron_studio::ControlValue;
    match value {
        ControlValue::Enum { value } | ControlValue::Resolution { value } => value.clone(),
        ControlValue::Integer { value } => value.to_string(),
        ControlValue::Number { value } | ControlValue::DurationSeconds { value } => {
            value.to_string()
        }
        ControlValue::Boolean { value } => if *value { "On" } else { "Off" }.into(),
        ControlValue::Dimensions { width, height } => format!("{width}×{height}"),
        ControlValue::AspectRatio { width, height } => format!("{width}:{height}"),
    }
}

async fn read_artifact_image(
    engine: &EngineHandle,
    artifact_id: StudioArtifactId,
) -> Result<Arc<Image>, String> {
    let (_, mime, bytes) = read_artifact_bytes(engine, artifact_id).await?;
    let format = ImageFormat::from_mime_type(&mime).unwrap_or(ImageFormat::Png);
    Ok(Arc::new(Image::from_bytes(format, bytes)))
}

async fn read_artifact_bytes(
    engine: &EngineHandle,
    artifact_id: StudioArtifactId,
) -> Result<(String, String, Vec<u8>), String> {
    let mut bytes = Vec::new();
    let mut offset = 0u64;
    for _ in 0..4096 {
        let value = engine
            .client()
            .call(
                methods::READ_STUDIO_ARTIFACT_CHUNK,
                serde_json::json!({ "artifactId": artifact_id, "offset": offset }),
            )
            .await
            .map_err(|error| error.to_string())?;
        let chunk: zeron_proto::StudioArtifactChunk =
            serde_json::from_value(value).map_err(|error| error.to_string())?;
        let mime = chunk.mime_type;
        bytes.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk.data)
                .map_err(|error| error.to_string())?,
        );
        if chunk.done {
            return Ok((chunk.file_name, mime, bytes));
        }
        if chunk.next_offset <= offset {
            return Err("artifact read stopped advancing".into());
        }
        offset = chunk.next_offset;
    }
    Err("artifact exceeded the chunk limit".into())
}

#[derive(Clone, Debug)]
pub enum StudioEvent {
    OpenProviders,
    SidebarChanged,
    OpenArtifact {
        conversation_id: StudioConversationId,
        artifact_id: StudioArtifactId,
    },
    CloseArtifact,
}

#[derive(Clone, Debug)]
struct DraftRunConfig {
    output_count: u32,
    controls: BTreeMap<zeron_studio::ControlId, zeron_studio::ControlValue>,
}

impl DraftRunConfig {
    fn from_model(model: &zeron_studio::MediaModel) -> Self {
        Self {
            output_count: 1,
            controls: model
                .controls
                .iter()
                .filter_map(|control| {
                    control
                        .default
                        .clone()
                        .map(|value| (control.id.clone(), value))
                })
                .collect(),
        }
    }
}

pub struct StudioPage {
    state: Entity<AppState>,
    conversations: Vec<StudioConversationSummary>,
    providers: Vec<StudioProviderConnection>,
    models: Vec<zeron_studio::MediaModel>,
    selected_models: BTreeSet<zeron_studio::ModelId>,
    draft_runs: HashMap<zeron_studio::ModelId, DraftRunConfig>,
    selected_conversation: Option<StudioConversationId>,
    conversation: Option<StudioConversationView>,
    prompt: Entity<ComposerInput>,
    model_search: Entity<ComposerInput>,
    model_picker: popover::Popup<()>,
    model_picker_active: Option<usize>,
    model_picker_scroll: gpui::ScrollHandle,
    model_picker_focus: FocusHandle,
    feed_scroll: gpui::ScrollHandle,
    artifact_filmstrip_scroll: gpui::ScrollHandle,
    scroll_after_turn_count: Option<usize>,
    scroll_task: Option<Task<()>>,
    rail_hover: Option<usize>,
    source_turn: Option<zeron_studio::StudioTurnId>,
    images: HashMap<StudioArtifactId, Arc<Image>>,
    loading_images: HashSet<StudioArtifactId>,
    selected_artifact: Option<StudioArtifactId>,
    lightbox_zoom: f32,
    lightbox_pan: Point<Pixels>,
    lightbox_drag: Option<Point<Pixels>>,
    lightbox_swipe_x: f32,
    lightbox_swipe_velocity: f32,
    lightbox_swipe_spring: bool,
    lightbox_swipe_scheduled: bool,
    lightbox_swipe_last_tick: Option<Instant>,
    filmstrip_scroll_accum: f32,
    focus: FocusHandle,
    loading: bool,
    busy: bool,
    error: Option<SharedString>,
    load_task: Option<Task<()>>,
    watch_task: Option<Task<()>>,
    action_task: Option<Task<()>>,
    image_tasks: HashMap<StudioArtifactId, Task<()>>,
    _observe: Subscription,
    _prompt_events: Subscription,
    _model_search_events: Subscription,
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
            lightbox_swipe_spring: false,
            lightbox_swipe_scheduled: false,
            lightbox_swipe_last_tick: None,
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

    fn engine(&self, cx: &Context<Self>) -> Option<EngineHandle> {
        self.state.read(cx).engine().cloned()
    }

    fn close_model_picker(&mut self, cx: &mut Context<Self>) {
        if self.model_picker.begin_close() {
            popover::reap_popup(cx, |page: &mut Self| &mut page.model_picker);
        }
        cx.notify();
    }

    fn toggle_model_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pressed_open = self.model_picker.take_press_was_open();
        if self.model_picker.is_open() || pressed_open {
            self.close_model_picker(cx);
            return;
        }

        self.model_picker.open(());
        self.model_search.update(cx, |input, cx| {
            input.set_placeholder("Search models…", cx);
            if !input.text().is_empty() {
                input.set_text("", cx);
            }
        });
        self.model_picker_active = None;
        self.model_picker_scroll.set_offset(Point::default());
        let search_focus = self.model_search.read(cx).focus_handle(cx);
        window.focus(&search_focus, cx);
        cx.notify();
    }

    fn filtered_model_indices(&self, cx: &gpui::App) -> Vec<usize> {
        let query = self.model_search.read(cx).text();
        let labels = self
            .models
            .iter()
            .map(|model| model.display_name.as_str())
            .collect::<Vec<_>>();
        popover::filter_indices(query, &labels)
    }

    fn activate_model_picker_row(&mut self, cx: &mut Context<Self>) {
        if !self.model_picker.is_open() {
            return;
        }
        let visible = self.filtered_model_indices(cx);
        let Some(model_index) = visible.get(self.model_picker_active.unwrap_or(0)).copied() else {
            return;
        };
        let id = self.models[model_index].id.clone();
        if !self.selected_models.remove(&id) {
            self.selected_models.insert(id);
        }
        cx.notify();
    }

    fn on_model_picker_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.model_picker.is_open() {
            return;
        }
        let key = popover::classify_key(
            event.keystroke.key.as_str(),
            event.keystroke.modifiers.platform,
            event.keystroke.modifiers.control,
        );
        let search_focus = self.model_search.read(cx).focus_handle(cx);
        let search_focused = search_focus.is_focused(window);
        let list_focused = self.model_picker_focus.is_focused(window);
        match key {
            popover::MenuKey::Escape => {
                self.close_model_picker(cx);
                cx.stop_propagation();
            }
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.filtered_model_indices(cx).len();
                if search_focused && key == popover::MenuKey::Down {
                    self.model_picker_active = (count > 0).then_some(0);
                    if count > 0 {
                        window.focus(&self.model_picker_focus, cx);
                    }
                } else if list_focused
                    && key == popover::MenuKey::Up
                    && self.model_picker_active == Some(0)
                {
                    self.model_picker_active = None;
                    window.focus(&search_focus, cx);
                } else if list_focused {
                    let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                    self.model_picker_active =
                        popover::menu_step(self.model_picker_active, count, delta);
                } else {
                    return;
                }
                if let Some(active) = self.model_picker_active {
                    self.model_picker_scroll.scroll_to_item(active);
                }
                cx.notify();
                cx.stop_propagation();
            }
            popover::MenuKey::Enter if list_focused => {
                self.activate_model_picker_row(cx);
                cx.stop_propagation();
            }
            _ if list_focused => {
                let modifiers = &event.keystroke.modifiers;
                if modifiers.platform || modifiers.control || modifiers.alt {
                    return;
                }
                let typed = event
                    .keystroke
                    .key_char
                    .as_deref()
                    .filter(|text| !text.is_empty())
                    .map(str::to_owned)
                    .or_else(|| {
                        let key = event.keystroke.key.as_str();
                        if key == "space" {
                            Some(" ".to_owned())
                        } else if key.chars().count() == 1 {
                            Some(key.to_owned())
                        } else {
                            None
                        }
                    });
                if let Some(typed) = typed {
                    let query = self.model_search.read(cx).text().to_owned();
                    window.focus(&search_focus, cx);
                    self.model_search.update(cx, |input, cx| {
                        input.set_text(format!("{query}{typed}"), cx)
                    });
                    cx.stop_propagation();
                } else if event.keystroke.key == "backspace" {
                    let mut query = self.model_search.read(cx).text().to_owned();
                    query.pop();
                    window.focus(&search_focus, cx);
                    self.model_search
                        .update(cx, |input, cx| input.set_text(query, cx));
                    cx.stop_propagation();
                }
            }
            _ => {}
        }
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

    fn start_missing_image_loads(&mut self, cx: &mut Context<Self>) {
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

    fn submit(&mut self, cx: &mut Context<Self>) {
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

    fn apply_models(&mut self, models: Vec<zeron_studio::MediaModel>) {
        self.models = models;
        for model in &self.models {
            self.draft_runs
                .entry(model.id.clone())
                .or_insert_with(|| DraftRunConfig::from_model(model));
        }
    }

    fn use_prompt(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
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

    fn generate_again(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
        self.use_prompt(turn, cx);
        self.submit(cx);
    }

    fn fork_from(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
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

    fn delete_artifact(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::DELETE_STUDIO_ARTIFACT,
                    serde_json::json!({ "artifactId": artifact_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.selected_artifact = None;
                        page.images.remove(&artifact_id);
                        cx.emit(StudioEvent::CloseArtifact);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn download_artifact(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let suggested = self
            .conversation
            .iter()
            .flat_map(|view| &view.turns)
            .flat_map(|turn| &turn.runs)
            .flat_map(|run| &run.artifacts)
            .find(|artifact| artifact.id == artifact_id)
            .map(|artifact| {
                let extension = match artifact.mime_type.as_str() {
                    "image/jpeg" => "jpg",
                    "image/webp" => "webp",
                    _ => "png",
                };
                format!("studio-{}.{}", artifact_id.0, extension)
            })
            .unwrap_or_else(|| format!("studio-{}.png", artifact_id.0));
        let receiver = cx.prompt_for_new_path(&PathBuf::new(), Some(&suggested));
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let destination = match receiver.await {
                Ok(Ok(Some(path))) => path,
                Ok(Ok(None)) => return,
                Ok(Err(error)) => {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
                Err(error) => {
                    this.update(cx, |page, cx| {
                        page.error = Some(error.to_string().into());
                        cx.notify();
                    })
                    .ok();
                    return;
                }
            };
            let result = match read_artifact_bytes(&engine, artifact_id).await {
                Ok((_, _, bytes)) => {
                    cx.background_executor()
                        .spawn(async move { write_artifact_file(destination, bytes) })
                        .await
                }
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                this.update(cx, |page, cx| {
                    page.error = Some(error.into());
                    cx.notify();
                })
                .ok();
            }
        }));
    }

    fn artifact_sequence(&self) -> Vec<StudioArtifactId> {
        self.conversation
            .iter()
            .flat_map(|view| &view.turns)
            .flat_map(|turn| &turn.runs)
            .flat_map(|run| &run.artifacts)
            .map(|artifact| artifact.id)
            .collect()
    }

    fn select_artifact_index(&mut self, index: usize, cx: &mut Context<Self>) -> bool {
        let artifacts = self.artifact_sequence();
        let Some(artifact_id) = artifacts.get(index).copied() else {
            return false;
        };
        let changed = self.selected_artifact != Some(artifact_id);
        self.selected_artifact = Some(artifact_id);
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        self.lightbox_drag = None;
        self.lightbox_swipe_x = 0.0;
        self.lightbox_swipe_velocity = 0.0;
        self.lightbox_swipe_spring = false;
        self.lightbox_swipe_last_tick = None;
        self.artifact_filmstrip_scroll.scroll_to_item(index);
        if let Some(conversation_id) = self.selected_conversation {
            cx.emit(StudioEvent::OpenArtifact {
                conversation_id,
                artifact_id,
            });
        }
        cx.notify();
        changed
    }

    fn navigate_artifact_with_wrap(
        &mut self,
        delta: isize,
        wraps: bool,
        cx: &mut Context<Self>,
    ) -> bool {
        let artifacts = self.artifact_sequence();
        let Some(selected) = self.selected_artifact else {
            return false;
        };
        let Some(index) = artifacts.iter().position(|id| *id == selected) else {
            return false;
        };
        let next = stepped_artifact_index(index, artifacts.len(), delta, wraps);
        if next == index {
            return false;
        }
        self.select_artifact_index(next, cx)
    }

    fn navigate_artifact(&mut self, delta: isize, cx: &mut Context<Self>) {
        self.navigate_artifact_with_wrap(delta, true, cx);
    }

    fn select_artifact_edge(&mut self, last: bool, cx: &mut Context<Self>) {
        let artifacts = self.artifact_sequence();
        self.selected_artifact = if last {
            artifacts.last().copied()
        } else {
            artifacts.first().copied()
        };
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        if let Some(index) = self.selected_artifact.and_then(|artifact| {
            artifacts
                .iter()
                .position(|candidate| *candidate == artifact)
        }) {
            self.artifact_filmstrip_scroll.scroll_to_item(index);
        }
        if let (Some(conversation_id), Some(artifact_id)) =
            (self.selected_conversation, self.selected_artifact)
        {
            cx.emit(StudioEvent::OpenArtifact {
                conversation_id,
                artifact_id,
            });
        }
        cx.notify();
    }

    pub fn show_artifact(
        &mut self,
        conversation_id: StudioConversationId,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        let mut changed = false;
        if self.selected_conversation != Some(conversation_id) {
            self.open_conversation(conversation_id, cx);
            changed = true;
        }
        if self.selected_artifact != Some(artifact_id) {
            self.selected_artifact = Some(artifact_id);
            self.lightbox_zoom = 1.0;
            self.lightbox_pan = Point::default();
            if let Some(index) = self
                .artifact_sequence()
                .iter()
                .position(|candidate| *candidate == artifact_id)
            {
                self.artifact_filmstrip_scroll.scroll_to_item(index);
            }
            changed = true;
        }
        if changed {
            cx.notify();
        }
    }

    pub fn close_artifact(&mut self, cx: &mut Context<Self>) {
        if self.selected_artifact.take().is_some() {
            self.lightbox_zoom = 1.0;
            self.lightbox_pan = Point::default();
            self.lightbox_drag = None;
            self.lightbox_swipe_x = 0.0;
            self.lightbox_swipe_velocity = 0.0;
            self.lightbox_swipe_spring = false;
            cx.notify();
        }
    }

    fn adjust_lightbox_zoom(&mut self, factor: f32, cx: &mut Context<Self>) {
        self.lightbox_zoom = (self.lightbox_zoom * factor).clamp(1.0, 8.0);
        cx.notify();
    }

    fn fit_lightbox(&mut self, cx: &mut Context<Self>) {
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        cx.notify();
    }

    fn begin_lightbox_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        if self.lightbox_zoom > 1.0 {
            self.lightbox_drag = Some(position);
            cx.notify();
        }
    }

    fn update_lightbox_pan(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(previous) = self.lightbox_drag else {
            return;
        };
        let limit = 420.0 * (self.lightbox_zoom - 1.0);
        self.lightbox_pan.x = px((f32::from(self.lightbox_pan.x)
            + f32::from(position.x - previous.x))
        .clamp(-limit, limit));
        self.lightbox_pan.y = px((f32::from(self.lightbox_pan.y)
            + f32::from(position.y - previous.y))
        .clamp(-limit, limit));
        self.lightbox_drag = Some(position);
        cx.notify();
    }

    fn end_lightbox_pan(&mut self, cx: &mut Context<Self>) {
        if self.lightbox_drag.take().is_some() {
            cx.notify();
        }
    }

    fn wake_lightbox_swipe_spring(&mut self, velocity: f32, cx: &mut Context<Self>) {
        self.lightbox_swipe_velocity = velocity;
        self.lightbox_swipe_spring = true;
        self.lightbox_swipe_last_tick = None;
        cx.notify();
    }

    fn step_lightbox_swipe_spring(&mut self, cx: &mut Context<Self>) {
        if !self.lightbox_swipe_spring {
            return;
        }
        let now = Instant::now();
        let frames = self
            .lightbox_swipe_last_tick
            .map(|last| (now.duration_since(last).as_secs_f32() * 60.0).clamp(0.25, 3.0))
            .unwrap_or(1.0);
        self.lightbox_swipe_last_tick = Some(now);
        (self.lightbox_swipe_x, self.lightbox_swipe_velocity) =
            step_artifact_swipe_spring(self.lightbox_swipe_x, self.lightbox_swipe_velocity, frames);
        if self.lightbox_swipe_x.abs() < 0.35 && self.lightbox_swipe_velocity.abs() < 0.35 {
            self.lightbox_swipe_x = 0.0;
            self.lightbox_swipe_velocity = 0.0;
            self.lightbox_swipe_spring = false;
            self.lightbox_swipe_last_tick = None;
        }
        cx.notify();
    }

    fn finish_lightbox_swipe(&mut self, cx: &mut Context<Self>) {
        let offset = self.lightbox_swipe_x;
        let release_velocity = self.lightbox_swipe_velocity;
        let delta = if offset > ARTIFACT_SWIPE_COMMIT {
            -1
        } else if offset < -ARTIFACT_SWIPE_COMMIT {
            1
        } else {
            0
        };
        let changed = delta != 0 && self.navigate_artifact_with_wrap(delta, false, cx);
        if changed {
            self.lightbox_swipe_x = -(offset.signum()) * 36.0;
        }
        self.wake_lightbox_swipe_spring(release_velocity * 0.35, cx);
    }

    fn on_lightbox_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(16.0));
        let horizontal = f32::from(delta.x);
        let vertical = f32::from(delta.y);
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled)
            && self.lightbox_zoom <= 1.001
            && self.lightbox_swipe_x.abs() > f32::EPSILON
        {
            self.finish_lightbox_swipe(cx);
            cx.stop_propagation();
            return;
        }
        if vertical.abs() > horizontal.abs() {
            let factor = (vertical * 0.004).exp();
            self.adjust_lightbox_zoom(factor, cx);
            if self.lightbox_zoom <= 1.001 {
                self.fit_lightbox(cx);
            }
            cx.stop_propagation();
            return;
        }
        if self.lightbox_zoom > 1.001 || horizontal.abs() < f32::EPSILON {
            cx.stop_propagation();
            return;
        }
        if event.touch_phase == TouchPhase::Started {
            self.lightbox_swipe_x = 0.0;
            self.lightbox_swipe_velocity = 0.0;
            self.lightbox_swipe_spring = false;
        }
        let artifacts = self.artifact_sequence();
        let index = self
            .selected_artifact
            .and_then(|selected| artifacts.iter().position(|id| *id == selected))
            .unwrap_or(0);
        let pushing_past_edge =
            (index == 0 && horizontal > 0.0) || (index + 1 == artifacts.len() && horizontal < 0.0);
        let applied = horizontal * if pushing_past_edge { 0.28 } else { 1.0 };
        self.lightbox_swipe_x =
            (self.lightbox_swipe_x + applied).clamp(-ARTIFACT_SWIPE_LIMIT, ARTIFACT_SWIPE_LIMIT);
        self.lightbox_swipe_velocity = applied;
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled)
            || (!event.delta.precise() && self.lightbox_swipe_x.abs() >= ARTIFACT_SWIPE_COMMIT)
        {
            self.finish_lightbox_swipe(cx);
        } else {
            cx.notify();
        }
        cx.stop_propagation();
    }

    fn on_lightbox_pinch(&mut self, event: &PinchEvent, cx: &mut Context<Self>) {
        self.adjust_lightbox_zoom((1.0 + event.delta).max(0.05), cx);
        if matches!(event.phase, TouchPhase::Ended | TouchPhase::Cancelled)
            && self.lightbox_zoom <= 1.01
        {
            self.fit_lightbox(cx);
        }
        cx.stop_propagation();
    }

    fn on_filmstrip_scroll(&mut self, event: &ScrollWheelEvent, cx: &mut Context<Self>) {
        let delta = event.delta.pixel_delta(px(16.0));
        let movement = if f32::from(delta.x).abs() > f32::EPSILON {
            f32::from(delta.x)
        } else {
            f32::from(delta.y)
        };
        if event.touch_phase == TouchPhase::Started {
            self.filmstrip_scroll_accum = 0.0;
        }
        self.filmstrip_scroll_accum += movement;
        while self.filmstrip_scroll_accum.abs() >= ARTIFACT_FILMSTRIP_STEP {
            let direction = if self.filmstrip_scroll_accum > 0.0 {
                -1
            } else {
                1
            };
            if !self.navigate_artifact_with_wrap(direction, false, cx) {
                self.filmstrip_scroll_accum *= 0.25;
                break;
            }
            self.filmstrip_scroll_accum -=
                self.filmstrip_scroll_accum.signum() * ARTIFACT_FILMSTRIP_STEP;
        }
        if matches!(event.touch_phase, TouchPhase::Ended | TouchPhase::Cancelled) {
            self.filmstrip_scroll_accum = 0.0;
        }
    }

    fn retry(
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

    fn adjust_output_count(
        &mut self,
        model_id: &zeron_studio::ModelId,
        delta: i32,
        maximum: u32,
        cx: &mut Context<Self>,
    ) {
        if let Some(draft) = self.draft_runs.get_mut(model_id) {
            draft.output_count =
                (draft.output_count as i32 + delta).clamp(1, maximum as i32) as u32;
            cx.notify();
        }
    }

    fn cycle_control(
        &mut self,
        model_id: &zeron_studio::ModelId,
        control: &zeron_studio::ModelControl,
        cx: &mut Context<Self>,
    ) {
        let Some(draft) = self.draft_runs.get_mut(model_id) else {
            return;
        };
        let current = draft.controls.get(&control.id).cloned();
        let next = if !control.choices.is_empty() {
            let index = control
                .choices
                .iter()
                .position(|choice| Some(&choice.value) == current.as_ref())
                .map(|index| (index + 1) % control.choices.len())
                .unwrap_or(0);
            Some(control.choices[index].value.clone())
        } else if control.kind == zeron_studio::ControlKind::Boolean {
            let value = match current {
                Some(zeron_studio::ControlValue::Boolean { value }) => !value,
                _ => !matches!(
                    control.default,
                    Some(zeron_studio::ControlValue::Boolean { value: true })
                ),
            };
            Some(zeron_studio::ControlValue::Boolean { value })
        } else if let Some(zeron_studio::ControlValue::Integer { value }) = current {
            let step = control.step.unwrap_or(1.0).max(1.0) as i64;
            let minimum = control.minimum.unwrap_or(value as f64) as i64;
            let maximum = control.maximum.unwrap_or((value + step) as f64) as i64;
            Some(zeron_studio::ControlValue::Integer {
                value: if value + step > maximum {
                    minimum
                } else {
                    value + step
                },
            })
        } else if let Some(zeron_studio::ControlValue::Number { value }) = current {
            let step = control.step.unwrap_or(1.0);
            let minimum = control.minimum.unwrap_or(value);
            let maximum = control.maximum.unwrap_or(value + step);
            Some(zeron_studio::ControlValue::Number {
                value: if value + step > maximum {
                    minimum
                } else {
                    value + step
                },
            })
        } else {
            None
        };
        if let Some(next) = next {
            draft.controls.insert(control.id.clone(), next);
            cx.notify();
        }
    }

    fn render_tile(
        &self,
        turn_ix: usize,
        run_ix: usize,
        output_ix: usize,
        width: f32,
        aspect: (u32, u32),
        state: StudioRunState,
        artifact_id: Option<StudioArtifactId>,
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
                    page.selected_artifact = Some(id);
                    page.lightbox_zoom = 1.0;
                    page.lightbox_pan = Point::default();
                    if let Some(conversation_id) = conversation_id {
                        cx.emit(StudioEvent::OpenArtifact {
                            conversation_id,
                            artifact_id: id,
                        });
                    }
                    window.focus(&page.focus, cx);
                    cx.notify();
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
        let label = match state {
            StudioRunState::Failed => "Generation failed",
            StudioRunState::Queued => "Queued",
            StudioRunState::Running => "Generating",
            StudioRunState::Downloading => "Downloading",
            _ => "Loading image",
        };
        let pending = matches!(
            state,
            StudioRunState::Queued | StudioRunState::Running | StudioRunState::Downloading
        );
        let shimmer = if pending && !crate::motion::reduced_motion(cx) {
            let phase = crate::motion::staggered_phase(
                crate::motion::pulse_delta(&crate::motion::ZERON_PULSE, cx.entity_id(), cx),
                turn_ix * 7 + run_ix * 3 + output_ix,
                0.07,
            );
            Some(
                div()
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(px((phase * 1.45 - 0.4) * width))
                    .w(px(width * 0.32))
                    .bg(gpui::white().opacity(0.035)),
            )
        } else {
            None
        };
        base.relative()
            .flex()
            .items_center()
            .justify_center()
            .text_size(px(11.0))
            .text_color(theme.text_faint)
            .when_some(shimmer, |tile, shimmer| tile.child(shimmer))
            .child(label)
            .into_any_element()
    }

    fn feed_turns(&self) -> &[StudioTurnView] {
        self.conversation
            .as_ref()
            .map(|view| view.turns.as_slice())
            .unwrap_or(&[])
    }

    fn rail_should_show(&self, container_width: f32) -> bool {
        rail::rail_visible(container_width) && self.feed_turns().len() >= 2
    }

    fn feed_container_width(&self, window: &Window) -> f32 {
        let measured = f32::from(self.feed_scroll.bounds().size.width);
        if measured > 0.0 {
            measured
        } else {
            (f32::from(window.viewport_size().width) - crate::settings::SIDEBAR_DEFAULT).max(0.0)
        }
    }

    /// Smooth-scroll the feed so `turn_ix` sits at the viewport top — same
    /// 500ms ease-in-out timeline as the chat MessageRail.
    fn scroll_to_turn(&mut self, turn_ix: usize, cx: &mut Context<Self>) {
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

    fn render_feed(
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
            .map(|(turn_ix, turn)| self.render_turn(turn_ix, turn, tile_width, gap, theme, cx))
            .collect()
    }

    fn render_turn(
        &mut self,
        turn_ix: usize,
        turn: &StudioTurnView,
        tile_width: f32,
        gap: f32,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turn_for_prompt = turn.clone();
        let turn_for_again = turn.clone();
        let turn_for_fork = turn.clone();
        let mut grid = div().w_full().flex().flex_row().flex_wrap().gap(px(gap));
        for (run_ix, run) in turn.runs.iter().enumerate() {
            for output_ix in 0..run.output_count as usize {
                let artifact = run
                    .artifacts
                    .iter()
                    .find(|artifact| artifact.output_position as usize == output_ix)
                    .map(|artifact| artifact.id);
                grid = grid.child(self.render_tile(
                    turn_ix,
                    run_ix,
                    output_ix,
                    tile_width,
                    run.display_aspect_ratio,
                    run.state,
                    artifact,
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
        div()
            .id(SharedString::from(format!("studio-turn-{turn_ix}")))
            .w_full()
            .max_w(px(1600.0))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(turn.prompt.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("studio-use-prompt-{turn_ix}")))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.use_prompt(&turn_for_prompt, cx)
                            }))
                            .child("Use prompt"),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "studio-generate-again-{turn_ix}"
                            )))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.generate_again(&turn_for_again, cx)
                            }))
                            .child("Generate again"),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("studio-fork-{turn_ix}")))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .on_click(
                                cx.listener(move |page, _, _, cx| {
                                    page.fork_from(&turn_for_fork, cx)
                                }),
                            )
                            .child("Fork"),
                    )
                    .children(retry_runs.into_iter().map(|(run_id, retry_anyway)| {
                        div()
                            .id(SharedString::from(format!("studio-retry-{}", run_id.0)))
                            .cursor_pointer()
                            .text_size(px(11.0))
                            .text_color(theme.danger)
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.retry(run_id, retry_anyway, cx)
                            }))
                            .child(if retry_anyway {
                                "Retry anyway"
                            } else {
                                "Retry"
                            })
                    })),
            )
            .child(grid)
            .into_any_element()
    }

    /// Left-edge tick rail for the conversation feed — same chrome as the
    /// chat MessageRail, with a Studio-specific hover card.
    fn render_studio_rail(
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

    fn render_composer(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let model_configs = self
            .models
            .clone()
            .into_iter()
            .filter(|model| self.selected_models.contains(&model.id))
            .map(|model| {
                let remove_id = model.id.clone();
                let output_count = self
                    .draft_runs
                    .get(&model.id)
                    .map(|draft| draft.output_count)
                    .unwrap_or(1);
                let maximum_output_count = model.maximum_output_count.max(1);
                let decrement_id = model.id.clone();
                let increment_id = model.id.clone();
                let aspect_control = model
                    .controls
                    .iter()
                    .find(|control| control.id.as_str() == "aspect_ratio")
                    .cloned();
                let resolution_control = model
                    .controls
                    .iter()
                    .find(|control| control.id.as_str() == "resolution")
                    .cloned();
                let reasoning_control = model
                    .controls
                    .iter()
                    .find(|control| control.id.as_str() == "reasoning")
                    .cloned();
                let draft = self
                    .draft_runs
                    .get(&model.id)
                    .cloned()
                    .unwrap_or_else(|| DraftRunConfig::from_model(&model));
                let aspect = draft_aspect(&model, &draft);
                let aspect_label = format!("{}:{}", aspect.0, aspect.1);
                let aspect_ratio = aspect.0 as f32 / aspect.1.max(1) as f32;
                let (indicator_w, indicator_h) = if aspect_ratio >= 1.0 {
                    (18.0, (18.0 / aspect_ratio).clamp(7.0, 18.0))
                } else {
                    ((18.0 * aspect_ratio).clamp(7.0, 18.0), 18.0)
                };
                let resolution_label = resolution_control
                    .as_ref()
                    .and_then(|control| draft.controls.get(&control.id))
                    .map(control_value_label)
                    .unwrap_or_else(|| "Auto".into());
                let reasoning_on = reasoning_control
                    .as_ref()
                    .and_then(|control| draft.controls.get(&control.id))
                    .is_some_and(|value| {
                        matches!(value, zeron_studio::ControlValue::Boolean { value: true })
                    });
                let aspect_model_id = model.id.clone();
                let resolution_model_id = model.id.clone();
                let reasoning_model_id = model.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "studio-model-config-{}",
                        model.id.as_str()
                    )))
                    .w(px(292.0))
                    .flex_none()
                    .rounded(px(10.0))
                    .border_1()
                    .border_color(theme.border)
                    .bg(crate::theme::wash(0.025))
                    .px(px(10.0))
                    .py(px(8.0))
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.0))
                                    .font_weight(gpui::FontWeight::MEDIUM)
                                    .child(SharedString::from(model.display_name.clone())),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "studio-remove-model-{}",
                                        remove_id.as_str()
                                    )))
                                    .size(px(18.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(5.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::wash(0.10)))
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.selected_models.remove(&remove_id);
                                        cx.notify();
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::CLOSE)
                                            .size(px(11.0))
                                            .text_color(theme.text_muted),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .flex()
                                    .items_center()
                                    .rounded(px(7.0))
                                    .bg(crate::theme::wash(0.07))
                                    .text_size(px(10.5))
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "studio-output-minus-{}",
                                                decrement_id.as_str()
                                            )))
                                            .cursor_pointer()
                                            .px(px(6.0))
                                            .py(px(5.0))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.adjust_output_count(
                                                    &decrement_id,
                                                    -1,
                                                    maximum_output_count,
                                                    cx,
                                                );
                                            }))
                                            .child("−"),
                                    )
                                    .child(
                                        div()
                                            .min_w(px(16.0))
                                            .text_center()
                                            .text_color(theme.text_muted)
                                            .child(SharedString::from(output_count.to_string())),
                                    )
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "studio-output-plus-{}",
                                                increment_id.as_str()
                                            )))
                                            .cursor_pointer()
                                            .px(px(6.0))
                                            .py(px(5.0))
                                            .on_click(cx.listener(move |page, _, _, cx| {
                                                page.adjust_output_count(
                                                    &increment_id,
                                                    1,
                                                    maximum_output_count,
                                                    cx,
                                                );
                                            }))
                                            .child("+"),
                                    ),
                            )
                            .when_some(aspect_control, |row, control| {
                                row.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "studio-aspect-{}",
                                            aspect_model_id.as_str()
                                        )))
                                        .h(px(27.0))
                                        .px(px(7.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .rounded(px(7.0))
                                        .bg(crate::theme::wash(0.07))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.cycle_control(&aspect_model_id, &control, cx)
                                        }))
                                        .child(
                                            div()
                                                .w(px(indicator_w))
                                                .h(px(indicator_h))
                                                .rounded(px(2.0))
                                                .border_1()
                                                .border_color(theme.text_muted.opacity(0.75)),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(theme.text_muted)
                                                .child(SharedString::from(aspect_label)),
                                        ),
                                )
                            })
                            .when_some(resolution_control, |row, control| {
                                row.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "studio-resolution-{}",
                                            resolution_model_id.as_str()
                                        )))
                                        .h(px(27.0))
                                        .px(px(7.0))
                                        .flex()
                                        .items_center()
                                        .rounded(px(7.0))
                                        .bg(crate::theme::wash(0.07))
                                        .cursor_pointer()
                                        .text_size(px(10.5))
                                        .text_color(theme.text_muted)
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.cycle_control(&resolution_model_id, &control, cx)
                                        }))
                                        .child(SharedString::from(resolution_label)),
                                )
                            })
                            .when_some(reasoning_control, |row, control| {
                                row.child(
                                    boolean_control_chip(
                                        format!("studio-reasoning-{}", reasoning_model_id.as_str()),
                                        "Reasoning",
                                        reasoning_on,
                                        theme,
                                    )
                                    .on_click(cx.listener(
                                        move |page, _, _, cx| {
                                            page.cycle_control(&reasoning_model_id, &control, cx)
                                        },
                                    )),
                                )
                            }),
                    )
            })
            .collect::<Vec<_>>();

        let visible_model_indices = self.filtered_model_indices(cx);
        let picker_rows = visible_model_indices
            .iter()
            .map(|model_index| self.models[*model_index].clone())
            .enumerate()
            .map(|(visible_index, model)| {
                let selected = self.selected_models.contains(&model.id);
                let active = self.model_picker_active == Some(visible_index);
                let id = model.id.clone();
                let mut row = div()
                    .id(SharedString::from(format!("studio-model-{}", id.as_str())))
                    .h(px(40.0))
                    .flex_none()
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(8.0))
                    .cursor_pointer()
                    .on_hover(cx.listener(move |page, hovered: &bool, _, cx| {
                        if *hovered && page.model_picker_active != Some(visible_index) {
                            page.model_picker_active = Some(visible_index);
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        if !page.selected_models.remove(&id) {
                            page.selected_models.insert(id.clone());
                        }
                        cx.notify();
                    }))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .child(SharedString::from(model.display_name)),
                    )
                    .when(selected, |row| {
                        row.child(
                            crate::icons::icon(crate::icons::CHECK)
                                .size(px(12.0))
                                .text_color(theme.text_muted),
                        )
                    });
                if selected {
                    row = row
                        .bg(crate::theme::card_selected_bg())
                        .shadow(crate::theme::card_selected_shadows());
                } else if active {
                    row = row.bg(crate::theme::ink(0.05));
                } else {
                    row = row.hover(|style| style.bg(crate::theme::ink(0.05)));
                }
                row
            })
            .collect::<Vec<_>>();

        let picker = self.model_picker.get().map(|_| {
            let empty = picker_rows.is_empty();
            let search_row = div()
                .flex_none()
                .h(px(46.0))
                .px(px(10.0))
                .border_b_1()
                .border_color(crate::theme::hairline(0.08))
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    crate::icons::icon(crate::icons::MAGNIFER)
                        .size(px(14.0))
                        .flex_none()
                        .text_color(theme.text_muted.opacity(0.7)),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(px(13.0))
                        .child(self.model_search.clone()),
                );
            let card = popover::popover_card_flush(theme)
                .id("studio-model-picker")
                .w(px(320.0))
                .track_focus(&self.model_picker_focus)
                .on_mouse_down_out(cx.listener(|page, _, _, cx| page.close_model_picker(cx)))
                .on_key_down(cx.listener(|page, event: &KeyDownEvent, window, cx| {
                    page.on_model_picker_key_down(event, window, cx)
                }))
                .child(search_row)
                .child(
                    div()
                        .id("studio-model-list")
                        .max_h(px(300.0))
                        .overflow_y_scroll()
                        .track_scroll(&self.model_picker_scroll)
                        .p(px(6.0))
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .when(empty, |list| {
                            list.child(
                                div()
                                    .h(px(72.0))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .text_size(px(12.0))
                                    .text_color(theme.text_muted)
                                    .child("No models found"),
                            )
                        })
                        .children(picker_rows),
                )
                .into_any_element();
            popover::anchored_menu_above(
                "studio-model-menu",
                card,
                self.model_picker.closing_since(),
            )
        });

        let blocked = self.busy
            || self.selected_models.is_empty()
            || self.prompt.read(cx).text().trim().is_empty();
        let composer = div()
            .w_full()
            .max_w(px(920.0))
            .occlude()
            .rounded(px(26.0))
            .border_1()
            .border_color(theme.border)
            .bg(theme.input_glass_bg())
            .when(!theme.is_glass(), |composer| composer.shadow_lg())
            .px(px(12.0))
            .pt(px(10.0))
            .pb(px(10.0))
            .flex()
            .flex_col()
            .gap(px(10.0))
            .child(
                div()
                    .id("studio-model-configs")
                    .flex()
                    .flex_row()
                    .gap(px(7.0))
                    .overflow_x_scroll()
                    .children(model_configs),
            )
            .child(
                div()
                    .min_h(px(54.0))
                    .px(px(4.0))
                    .py(px(4.0))
                    .child(self.prompt.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .relative()
                            .when_some(picker, |button, picker| button.child(picker))
                            .child(
                                div()
                                    .id("studio-model-picker-toggle")
                                    .h(px(28.0))
                                    .px(px(8.0))
                                    .flex()
                                    .items_center()
                                    .gap(px(6.0))
                                    .rounded(px(7.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::wash(0.08)))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(|page, _, _, _| {
                                            page.model_picker.note_trigger_press()
                                        }),
                                    )
                                    .on_click(cx.listener(|page, _, window, cx| {
                                        page.toggle_model_picker(window, cx)
                                    }))
                                    .child(
                                        crate::icons::icon(crate::icons::WIDGET)
                                            .size(px(13.0))
                                            .text_color(theme.text_muted),
                                    )
                                    .child(
                                        div()
                                            .text_size(px(11.0))
                                            .font_weight(gpui::FontWeight::MEDIUM)
                                            .child(SharedString::from(format!(
                                                "{} model{}",
                                                self.selected_models.len(),
                                                if self.selected_models.len() == 1 {
                                                    ""
                                                } else {
                                                    "s"
                                                }
                                            ))),
                                    ),
                            ),
                    )
                    .when(self.source_turn.is_some(), |row| {
                        row.child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme.text_faint)
                                .child("Using previous settings"),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("studio-generate")
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
                                    .on_click(cx.listener(|page, _, _, cx| page.submit(cx)))
                            })
                            .child(
                                crate::icons::icon(crate::icons::ARROW_UP)
                                    .size(px(14.0))
                                    .text_color(theme.bg),
                            ),
                    ),
            );

        div()
            .absolute()
            .left(px(24.0))
            .right(px(24.0))
            .bottom(px(18.0))
            .flex()
            .justify_center()
            .child(crate::frost::frosted(26.0, 16.0, composer))
            .into_any_element()
    }

    fn render_artifact_page(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.selected_artifact?;
        let image = self.images.get(&id).cloned();
        let sequence = self.artifact_sequence();
        let selected_index = sequence
            .iter()
            .position(|candidate| *candidate == id)
            .unwrap_or(0);
        let details = self.conversation.as_ref().and_then(|view| {
            view.turns.iter().find_map(|turn| {
                turn.runs.iter().find_map(|run| {
                    run.artifacts
                        .iter()
                        .find(|artifact| artifact.id == id)
                        .map(|artifact| {
                            (
                                turn.prompt.clone(),
                                run.model.display_name.clone(),
                                artifact.mime_type.clone(),
                                artifact.size_bytes,
                            )
                        })
                })
            })
        });
        let thumbnails = sequence
            .iter()
            .enumerate()
            .map(|(index, artifact_id)| {
                let thumbnail = self.images.get(artifact_id).cloned();
                let frame_size = if index == selected_index { 58.0 } else { 50.0 };
                div()
                    .id(SharedString::from(format!("studio-thumbnail-{index}")))
                    .size(px(frame_size))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(8.0))
                    .overflow_hidden()
                    .border_1()
                    .border_color(if index == selected_index {
                        theme.text_muted
                    } else {
                        theme.border
                    })
                    .bg(crate::theme::wash(0.04))
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.82))
                    .on_click(cx.listener(move |page, _, _, cx| {
                        page.select_artifact_index(index, cx);
                    }))
                    .when_some(thumbnail, |thumb, thumbnail| {
                        thumb.child(
                            div()
                                .size(px(frame_size - 2.0))
                                .flex_none()
                                .rounded(px(7.0))
                                .overflow_hidden()
                                .child(
                                    img(thumbnail)
                                        .size_full()
                                        .rounded(px(7.0))
                                        .object_fit(ObjectFit::Cover),
                                ),
                        )
                    })
            })
            .collect::<Vec<_>>();

        let stage_image = if let Some(image) = image {
            div()
                .id("studio-artifact-image")
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .relative()
                .left(self.lightbox_pan.x + px(self.lightbox_swipe_x))
                .top(self.lightbox_pan.y)
                .cursor_pointer()
                .on_scroll_wheel(
                    cx.listener(|page, event, _, cx| page.on_lightbox_scroll(event, cx)),
                )
                .on_pinch(cx.listener(|page, event, _, cx| page.on_lightbox_pinch(event, cx)))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                        page.begin_lightbox_pan(event.position, cx);
                    }),
                )
                .on_mouse_move(cx.listener(|page, event: &gpui::MouseMoveEvent, _, cx| {
                    if event.dragging() {
                        page.update_lightbox_pan(event.position, cx);
                    }
                }))
                .on_mouse_up(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.end_lightbox_pan(cx)),
                )
                .on_mouse_up_out(
                    gpui::MouseButton::Left,
                    cx.listener(|page, _, _, cx| page.end_lightbox_pan(cx)),
                )
                .on_click(cx.listener(|page, event: &gpui::ClickEvent, _, cx| {
                    if event.click_count() == 2 {
                        if page.lightbox_zoom > 1.0 {
                            page.fit_lightbox(cx);
                        } else {
                            page.adjust_lightbox_zoom(2.0, cx);
                        }
                    }
                }))
                .child(
                    img(image)
                        .w(gpui::relative(self.lightbox_zoom))
                        .h(gpui::relative(self.lightbox_zoom))
                        .object_fit(ObjectFit::Contain),
                )
                .into_any_element()
        } else {
            div()
                .text_size(px(12.0))
                .text_color(theme.text_faint)
                .child("Loading image…")
                .into_any_element()
        };

        let back_button = div()
            .id("studio-artifact-back")
            .size(px(24.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.14)))
            .on_click(cx.listener(|page, _, _, cx| {
                page.selected_artifact = None;
                cx.emit(StudioEvent::CloseArtifact);
                cx.notify();
            }))
            .child(
                crate::icons::icon(crate::icons::ARROW_LEFT)
                    .size(px(14.0))
                    .text_color(theme.text_muted.opacity(0.7)),
            );

        let previous = div()
            .id("studio-artifact-previous")
            .size(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.11)))
            .on_click(cx.listener(|page, _, _, cx| page.navigate_artifact(-1, cx)))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_LEFT)
                    .size(px(18.0))
                    .text_color(theme.text_muted),
            );
        let next = div()
            .id("studio-artifact-next")
            .size(px(32.0))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(7.0))
            .cursor_pointer()
            .hover(|style| style.bg(crate::theme::wash(0.11)))
            .on_click(cx.listener(|page, _, _, cx| page.navigate_artifact(1, cx)))
            .child(
                crate::icons::icon(crate::icons::ALT_ARROW_RIGHT)
                    .size(px(18.0))
                    .text_color(theme.text_muted),
            );

        let inspector = div()
            .size_full()
            .flex()
            .flex_col()
            .border_l_1()
            .border_color(theme.border)
            .bg(theme.glass_overlay())
            .px(px(18.0))
            .pt(px(Theme::TITLEBAR_HEIGHT + 18.0))
            .pb(px(16.0))
            .when_some(details, |inspector, (prompt, model, mime, size)| {
                let copy_prompt = prompt.clone();
                inspector
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_size(px(12.0))
                                    .line_height(px(18.0))
                                    .text_color(theme.text)
                                    .child(SharedString::from(prompt)),
                            )
                            .child(
                                div()
                                    .id("studio-copy-prompt")
                                    .size(px(24.0))
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .rounded(px(6.0))
                                    .cursor_pointer()
                                    .hover(|style| style.bg(crate::theme::wash(0.14)))
                                    .on_click(move |_, _, cx| {
                                        cx.write_to_clipboard(ClipboardItem::new_string(
                                            copy_prompt.clone(),
                                        ));
                                    })
                                    .child(
                                        crate::icons::icon(crate::icons::COPY)
                                            .size(px(14.0))
                                            .text_color(theme.text_muted.opacity(0.7)),
                                    ),
                            ),
                    )
                    .child(
                        div()
                            .mt(px(14.0))
                            .text_size(px(11.0))
                            .text_color(theme.text_muted)
                            .child(SharedString::from(format!(
                                "{model} · {mime} · {:.1} KB",
                                size as f64 / 1024.0
                            ))),
                    )
            })
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .id("studio-download-artifact")
                            .h(px(32.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(7.0))
                            .rounded(px(7.0))
                            .border_1()
                            .border_color(theme.border)
                            .cursor_pointer()
                            .hover(|style| style.bg(crate::theme::wash(0.09)))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.download_artifact(id, cx)),
                            )
                            .child(
                                crate::icons::icon(crate::icons::ARROW_DOWN)
                                    .size(px(14.0))
                                    .text_color(theme.text_muted),
                            )
                            .child("Download"),
                    )
                    .child(
                        div()
                            .id("studio-delete-artifact")
                            .h(px(32.0))
                            .flex_1()
                            .flex()
                            .items_center()
                            .justify_center()
                            .gap(px(7.0))
                            .rounded(px(7.0))
                            .cursor_pointer()
                            .text_color(theme.danger)
                            .hover(|style| style.bg(theme.danger.opacity(0.08)))
                            .on_click(
                                cx.listener(move |page, _, _, cx| page.delete_artifact(id, cx)),
                            )
                            .child(
                                crate::icons::icon(crate::icons::TRASH_BIN_MINIMALISTIC)
                                    .size(px(14.0))
                                    .text_color(theme.danger),
                            )
                            .child("Delete"),
                    ),
            );

        Some(
            div()
                .size_full()
                .flex()
                .min_w_0()
                .track_focus(&self.focus)
                .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, _, cx| {
                    match event.keystroke.key.as_str() {
                        "escape" => {
                            page.selected_artifact = None;
                            cx.emit(StudioEvent::CloseArtifact);
                            cx.notify();
                        }
                        "left" => page.navigate_artifact(-1, cx),
                        "right" => page.navigate_artifact(1, cx),
                        "home" => page.select_artifact_edge(false, cx),
                        "end" => page.select_artifact_edge(true, cx),
                        _ => {}
                    }
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .h_full()
                        .relative()
                        .flex()
                        .flex_col()
                        .overflow_hidden()
                        .child(
                            div()
                                .h(px(Theme::TITLEBAR_HEIGHT))
                                .flex_none()
                                .flex()
                                .items_center()
                                .pt(px(Theme::TITLEBAR_TOP_PAD))
                                .px(px(16.0))
                                .child(back_button),
                        )
                        .child(
                            div()
                                .id("studio-artifact-stage")
                                .flex_1()
                                .min_h_0()
                                .flex()
                                .items_center()
                                .gap(px(12.0))
                                .px(px(16.0))
                                .child(previous)
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .h_full()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .overflow_hidden()
                                        .child(stage_image),
                                )
                                .child(next),
                        )
                        .child(
                            div()
                                .id("studio-artifact-filmstrip")
                                .h(px(78.0))
                                .flex_none()
                                .overflow_x_scroll()
                                .track_scroll(&self.artifact_filmstrip_scroll)
                                .on_scroll_wheel(cx.listener(|page, event, _, cx| {
                                    page.on_filmstrip_scroll(event, cx)
                                }))
                                .flex()
                                .items_center()
                                .justify_center()
                                .gap(px(8.0))
                                .px(px(16.0))
                                .children(thumbnails),
                        ),
                )
                .child(
                    div()
                        .w(px(320.0))
                        .h_full()
                        .flex_none()
                        .child(crate::frost::frosted(
                            0.0,
                            crate::frost::MENU_BLUR,
                            inspector,
                        )),
                )
                .into_any_element(),
        )
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
        if self.lightbox_swipe_spring && crate::motion::reduced_motion(cx) {
            self.lightbox_swipe_x = 0.0;
            self.lightbox_swipe_velocity = 0.0;
            self.lightbox_swipe_spring = false;
            self.lightbox_swipe_last_tick = None;
        } else if self.lightbox_swipe_spring && !self.lightbox_swipe_scheduled {
            self.lightbox_swipe_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |page: &mut StudioPage, cx| {
                        page.lightbox_swipe_scheduled = false;
                        page.step_lightbox_swipe_spring(cx);
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

pub struct ProvidersPage {
    state: Entity<AppState>,
    providers: Vec<StudioProviderConnection>,
    secret: Entity<ComposerInput>,
    loading: bool,
    error: Option<SharedString>,
    task: Option<Task<()>>,
    _observe: Subscription,
}

impl ProvidersPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let secret = cx.new(|cx| ComposerInput::new("Provider API key", cx));
        let mut page = Self {
            state,
            providers: Vec::new(),
            secret,
            loading: false,
            error: None,
            task: None,
            _observe: observe,
        };
        page.load(cx);
        page
    }

    fn load(&mut self, cx: &mut Context<Self>) {
        self.error = None;
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            self.error = Some("Engine not connected".into());
            return;
        };
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(methods::LIST_STUDIO_PROVIDERS, serde_json::json!({}))
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<ListStudioProvidersResponse>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(value) => {
                        page.providers = value.providers;
                        page.revalidate_configured(cx);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn revalidate_configured(&mut self, cx: &mut Context<Self>) {
        let ids = self
            .providers
            .iter()
            .filter(|provider| {
                provider.configured && provider.validation_state != ProviderValidationState::Valid
            })
            .map(|provider| provider.provider_id.clone())
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return;
        }
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.task =
            Some(cx.spawn(async move |this, cx| {
                for provider_id in ids {
                    let result = engine
                        .client()
                        .call(
                            methods::VALIDATE_STUDIO_PROVIDER,
                            serde_json::json!({ "providerId": provider_id }),
                        )
                        .await;
                    let stop = this
                        .update(cx, |page, cx| {
                            match result.and_then(|value| {
                                serde_json::from_value::<StudioProviderConnection>(value)
                                    .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                            }) {
                                Ok(connection) => {
                                    if let Some(slot) = page.providers.iter_mut().find(|provider| {
                                        provider.provider_id == connection.provider_id
                                    }) {
                                        *slot = connection;
                                    }
                                }
                                Err(error) => page.error = Some(error.to_string().into()),
                            }
                            cx.notify();
                        })
                        .is_err();
                    if stop {
                        break;
                    }
                }
            }));
    }

    fn save(&mut self, provider_id: zeron_studio::ProviderId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        let secret = self.secret.read(cx).text().trim().to_owned();
        if secret.is_empty() {
            return;
        }
        let display_label = self
            .providers
            .iter()
            .find(|provider| provider.provider_id == provider_id)
            .map(|provider| provider.display_label.clone())
            .unwrap_or_else(|| "Image provider".to_owned());
        self.error = None;
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = async {
                engine.client().call(methods::SET_STUDIO_PROVIDER_CREDENTIAL, serde_json::json!({ "providerId": provider_id, "displayLabel": display_label, "secret": secret })).await?;
                engine.client().call(methods::VALIDATE_STUDIO_PROVIDER, serde_json::json!({ "providerId": provider_id })).await
            }.await;
            this.update(cx, |page, cx| { page.loading = false; match result.and_then(|value| serde_json::from_value::<StudioProviderConnection>(value).map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))) { Ok(connection) => { if let Some(slot) = page.providers.iter_mut().find(|provider| provider.provider_id == connection.provider_id) { *slot = connection; } page.secret.update(cx, |input, cx| input.set_text("", cx)); }, Err(error) => page.error = Some(error.to_string().into()) } cx.notify(); }).ok();
        }));
    }

    fn set_safe_mode(
        &mut self,
        provider_id: zeron_studio::ProviderId,
        safe_mode: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::SET_STUDIO_PROVIDER_PREFERENCES,
                    serde_json::json!({ "providerId": provider_id, "safeMode": safe_mode }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                match result.and_then(|value| {
                    serde_json::from_value::<StudioProviderConnection>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(connection) => {
                        if let Some(slot) = page
                            .providers
                            .iter_mut()
                            .find(|provider| provider.provider_id == connection.provider_id)
                        {
                            *slot = connection;
                        }
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn remove(&mut self, provider_id: zeron_studio::ProviderId, cx: &mut Context<Self>) {
        let Some(engine) = self.state.read(cx).engine().cloned() else {
            return;
        };
        self.error = None;
        self.loading = true;
        self.task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::REMOVE_STUDIO_PROVIDER_CREDENTIAL,
                    serde_json::json!({ "providerId": provider_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.loading = false;
                match result {
                    Ok(_) => page.load(cx),
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn render_provider_section(
        &mut self,
        provider: StudioProviderConnection,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = provider.provider_id.clone();
        let remove_id = provider.provider_id.clone();
        let toggle_id = provider.provider_id.clone();
        let configured = provider.configured;
        let safe_mode = provider.safe_mode;
        let venice = provider.provider_id.as_str() == "venice";
        let validation_message = provider
            .validation_message
            .clone()
            .filter(|_| provider.validation_state != ProviderValidationState::Valid);
        let label = if provider.display_label.eq_ignore_ascii_case("venice") {
            "Venice".to_owned()
        } else {
            provider.display_label.clone()
        };
        let (status, status_badge) = match provider.validation_state {
            ProviderValidationState::Valid => (
                "Connected",
                widgets::badge_active(theme, "Connected").into_any_element(),
            ),
            ProviderValidationState::Invalid => (
                "Invalid key",
                widgets::badge(theme, "Invalid key").into_any_element(),
            ),
            ProviderValidationState::Unavailable => (
                "Unavailable",
                widgets::badge(theme, "Unavailable").into_any_element(),
            ),
            ProviderValidationState::NotValidated if configured => (
                "Not validated",
                widgets::badge(theme, "Not validated").into_any_element(),
            ),
            _ => (
                "Not connected",
                widgets::badge(theme, "Not connected").into_any_element(),
            ),
        };
        let save_label = if self.loading {
            "Working…"
        } else if configured {
            "Save and validate"
        } else {
            "Add key"
        };

        let mut card =
            widgets::section_card(theme).mt(px(8.0)).child(
                widgets::card_row(theme, true)
                    .child(widgets::row_tile(theme, icons::KEY_MINIMALISTIC))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "API key"))
                            .child(widgets::meta_line(
                                theme,
                                vec![div().child(SharedString::from(status)).into_any_element()],
                            )),
                    )
                    .child(status_badge)
                    .when(configured, |row| {
                        row.child(
                            widgets::ghost_action(theme)
                                .id(SharedString::from(format!(
                                    "provider-remove-{}",
                                    remove_id.as_str()
                                )))
                                .hover(|style| widgets::ghost_hover(theme, style))
                                .on_click(cx.listener(move |page, _, _, cx| {
                                    page.remove(remove_id.clone(), cx)
                                }))
                                .child(
                                    icons::icon(icons::TRASH_BIN_MINIMALISTIC)
                                        .size(px(16.0))
                                        .text_color(theme.text_muted),
                                )
                                .child(SharedString::from("Remove")),
                        )
                    }),
            );
        card = card.child(
            div()
                .px(px(20.0))
                .pb(px(14.0))
                .flex()
                .flex_col()
                .gap(px(10.0))
                .child(popover::dialog_field(
                    self.secret.clone().into_any_element(),
                ))
                .child(
                    div().flex().justify_end().child(
                        popover::btn_primary(theme, save_label)
                            .id(SharedString::from(format!("provider-save-{}", id.as_str())))
                            .when(self.loading, |el| el.opacity(0.5))
                            .on_click(cx.listener(move |page, _, _, cx| page.save(id.clone(), cx))),
                    ),
                ),
        );
        if configured && venice {
            card = card.child(
                widgets::card_row(theme, false)
                    .child(widgets::row_tile(theme, icons::TUNING))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(widgets::row_title(theme, "Safe mode"))
                            .child(widgets::meta_line(
                                theme,
                                vec![
                                    div()
                                        .child(SharedString::from(
                                            "Blur images Venice classifies as adult content.",
                                        ))
                                        .into_any_element(),
                                ],
                            )),
                    )
                    .child(
                        widgets::toggle_switch(theme, safe_mode)
                            .id(SharedString::from(format!(
                                "provider-safe-mode-{}",
                                toggle_id.as_str()
                            )))
                            .cursor_pointer()
                            .on_click(cx.listener(move |page, _, _, cx| {
                                page.set_safe_mode(toggle_id.clone(), !safe_mode, cx)
                            })),
                    ),
            );
        }

        div()
            .mt(px(24.0))
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
                            .flex_none()
                            .size(px(24.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(
                                icons::icon(icons::KEY_MINIMALISTIC)
                                    .size(px(16.0))
                                    .text_color(theme.text_muted),
                            ),
                    )
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(gpui::FontWeight::MEDIUM)
                            .text_color(theme.text)
                            .child(SharedString::from(label)),
                    ),
            )
            .when_some(validation_message, |section, message| {
                section.child(widgets::warning_strip(theme, message))
            })
            .child(card)
            .into_any_element()
    }
}

impl Render for ProvidersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let configured_count = {
            let count = self
                .providers
                .iter()
                .filter(|provider| provider.configured)
                .count();
            (count > 0).then_some(count)
        };
        let sections: Vec<AnyElement> = if self.loading && self.providers.is_empty() {
            vec![
                widgets::section_card(&theme)
                    .p(px(16.0))
                    .child(popover::skeleton_rows(
                        "providers-skeleton",
                        &theme,
                        2,
                        cx.entity_id(),
                        cx,
                    ))
                    .into_any_element(),
            ]
        } else {
            self.providers
                .clone()
                .into_iter()
                .map(|provider| self.render_provider_section(provider, &theme, cx))
                .collect()
        };

        div()
            .id("providers-page")
            .size_full()
            .overflow_y_scroll()
            .child(
                widgets::page_column()
                    .child(widgets::page_header(&theme, "Providers", configured_count))
                    .child(widgets::page_subtitle(
                        &theme,
                        "Image-generation accounts on this device. Keys stay in the platform \
                         secret store and are never synced.",
                    ))
                    .when_some(self.error.clone(), |el, message| {
                        el.child(
                            widgets::error_strip(&theme, message)
                                .id("providers-action-error")
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        )
                    })
                    .children(sections)
                    .child(
                        div()
                            .mt(px(24.0))
                            .text_size(px(12.0))
                            .line_height(px(19.0))
                            .text_color(theme.text_muted.opacity(0.6))
                            .child(SharedString::from(
                                "Safe mode is a Venice setting. When it is on, images Venice \
                                 classifies as adult content come back blurred. It stays off \
                                 unless you turn it on.",
                            )),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn artifact_navigation_wraps_for_arrows_but_clamps_for_swipes() {
        assert_eq!(stepped_artifact_index(0, 4, -1, true), 3);
        assert_eq!(stepped_artifact_index(3, 4, 1, true), 0);
        assert_eq!(stepped_artifact_index(0, 4, -1, false), 0);
        assert_eq!(stepped_artifact_index(3, 4, 1, false), 3);
        assert_eq!(stepped_artifact_index(1, 4, 1, false), 2);
    }

    #[test]
    fn artifact_swipe_spring_settles_after_edge_resistance() {
        let (mut position, mut velocity) = (80.0, 0.0);
        for _ in 0..120 {
            (position, velocity) = step_artifact_swipe_spring(position, velocity, 1.0);
        }
        assert!(position.abs() < 0.35);
        assert!(velocity.abs() < 0.35);
    }

    #[test]
    fn artifact_file_write_does_not_require_a_tokio_runtime() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let destination = dir.path().join("download.png");

        write_artifact_file(destination.clone(), b"image bytes".to_vec()).expect("artifact write");

        assert_eq!(
            std::fs::read(destination).expect("saved artifact"),
            b"image bytes"
        );
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
            manifest_version: "test".into(),
            fetched_at: Utc::now(),
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
