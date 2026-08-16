//! First-release Studio viewport and provider settings.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine as _;
use gpui::{
    AnyElement, Context, Entity, EventEmitter, FocusHandle, Focusable, Image, ImageFormat,
    KeyDownEvent, ObjectFit, Pixels, Point, Render, SharedString, Subscription, Task, Window, div,
    img, prelude::*, px,
};
use zeron_proto::{
    ListStudioConversationsResponse, ListStudioModelsResponse, ListStudioProvidersResponse,
    ProviderValidationState, StudioConversationSummary, StudioConversationView,
    StudioProviderConnection, StudioRunState, StudioTurnView,
};
use zeron_rpc::methods;
use zeron_studio::{StudioArtifactId, StudioConversationId};

use crate::composer::{ComposerInput, ComposerInputEvent};
use crate::popover;
use crate::state::{AppState, EngineHandle};
use crate::theme::Theme;

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
    source_turn: Option<zeron_studio::StudioTurnId>,
    images: HashMap<StudioArtifactId, Arc<Image>>,
    loading_images: HashSet<StudioArtifactId>,
    selected_artifact: Option<StudioArtifactId>,
    lightbox_zoom: f32,
    lightbox_pan: Point<Pixels>,
    lightbox_drag: Option<Point<Pixels>>,
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
        let model_search = cx.new(|cx| ComposerInput::new("Search models…", cx));
        let prompt_events = cx.subscribe(&prompt, |page: &mut Self, _, event, cx| match event {
            ComposerInputEvent::Submitted => page.submit(cx),
            ComposerInputEvent::Edited => cx.notify(),
            _ => {}
        });
        let model_search_events =
            cx.subscribe(&model_search, |page: &mut Self, _, event, cx| match event {
                ComposerInputEvent::Edited => {
                    page.model_picker_active =
                        (!page.filtered_model_indices(cx).is_empty()).then_some(0);
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
            source_turn: None,
            images: HashMap::new(),
            loading_images: HashSet::new(),
            selected_artifact: None,
            lightbox_zoom: 1.0,
            lightbox_pan: Point::default(),
            lightbox_drag: None,
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
        let visible = self.filtered_model_indices(cx);
        self.model_picker_active = visible
            .iter()
            .position(|index| self.selected_models.contains(&self.models[*index].id))
            .or((!visible.is_empty()).then_some(0));
        self.model_picker_scroll.set_offset(Point::default());
        if let Some(active) = self.model_picker_active {
            self.model_picker_scroll.scroll_to_item(active);
        }
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
        let Some(model_index) = self
            .model_picker_active
            .and_then(|active| visible.get(active))
            .copied()
        else {
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
        window: &Window,
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
        match key {
            popover::MenuKey::Escape => self.close_model_picker(cx),
            popover::MenuKey::Up | popover::MenuKey::Down => {
                let count = self.filtered_model_indices(cx).len();
                let delta = if key == popover::MenuKey::Up { -1 } else { 1 };
                self.model_picker_active =
                    popover::menu_step(self.model_picker_active, count, delta);
                if let Some(active) = self.model_picker_active {
                    self.model_picker_scroll.scroll_to_item(active);
                }
                cx.notify();
            }
            popover::MenuKey::Enter
                if !self
                    .model_search
                    .read(cx)
                    .focus_handle(cx)
                    .is_focused(window) =>
            {
                self.activate_model_picker_row(cx)
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
                        page.models = models.models;
                        for model in &page.models {
                            page.draft_runs
                                .entry(model.id.clone())
                                .or_insert_with(|| DraftRunConfig::from_model(model));
                        }
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
                        page.conversation = Some(view);
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
            this.update(cx, |page, cx| {
                page.busy = false;
                match result {
                    Ok(_) => {
                        page.prompt.update(cx, |input, cx| input.set_text("", cx));
                        page.source_turn = None;
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
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
                Ok((_, _, bytes)) => tokio::fs::write(destination, bytes)
                    .await
                    .map_err(|error| error.to_string()),
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

    fn navigate_artifact(&mut self, delta: isize, cx: &mut Context<Self>) {
        let artifacts = self.artifact_sequence();
        let Some(selected) = self.selected_artifact else {
            return;
        };
        let Some(index) = artifacts.iter().position(|id| *id == selected) else {
            return;
        };
        if artifacts.is_empty() {
            return;
        }
        let next = (index as isize + delta).rem_euclid(artifacts.len() as isize) as usize;
        self.selected_artifact = Some(artifacts[next]);
        self.lightbox_zoom = 1.0;
        self.lightbox_pan = Point::default();
        if let Some(conversation_id) = self.selected_conversation {
            cx.emit(StudioEvent::OpenArtifact {
                conversation_id,
                artifact_id: artifacts[next],
            });
        }
        cx.notify();
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
        } else if let Some(zeron_studio::ControlValue::Boolean { value }) = current {
            Some(zeron_studio::ControlValue::Boolean { value: !value })
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

    fn render_feed(
        &mut self,
        window: &Window,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let turns = self
            .conversation
            .clone()
            .map(|view| view.turns)
            .unwrap_or_default();
        if turns.is_empty() {
            return div()
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
                .into_any_element();
        }

        let available =
            (f32::from(window.viewport_size().width) - 256.0 - 240.0 - 64.0).clamp(240.0, 1600.0);
        let columns = grid_columns(available);
        let gap = if available < 520.0 { 12.0 } else { 16.0 };
        let tile_width = (available - gap * (columns.saturating_sub(1) as f32)) / columns as f32;
        let mut feed = div()
            .w_full()
            .max_w(px(1600.0))
            .mx_auto()
            .flex()
            .flex_col()
            .gap(px(28.0))
            .pb(px(190.0));
        for (turn_ix, turn) in turns.iter().enumerate() {
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
            feed = feed.child(
                div()
                    .id(SharedString::from(format!("studio-turn-{turn_ix}")))
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
                                    .on_click(cx.listener(move |page, _, _, cx| {
                                        page.fork_from(&turn_for_fork, cx)
                                    }))
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
                    .child(grid),
            );
        }
        feed.into_any_element()
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
                                    div()
                                        .id(SharedString::from(format!(
                                            "studio-reasoning-{}",
                                            reasoning_model_id.as_str()
                                        )))
                                        .h(px(27.0))
                                        .px(px(7.0))
                                        .flex()
                                        .items_center()
                                        .gap(px(5.0))
                                        .rounded(px(7.0))
                                        .bg(crate::theme::wash(if reasoning_on {
                                            0.12
                                        } else {
                                            0.07
                                        }))
                                        .cursor_pointer()
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.cycle_control(&reasoning_model_id, &control, cx)
                                        }))
                                        .child(
                                            div()
                                                .w(px(18.0))
                                                .h(px(10.0))
                                                .rounded_full()
                                                .bg(if reasoning_on {
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
                                                        .when(reasoning_on, |dot| dot.ml(px(8.0))),
                                                ),
                                        )
                                        .child(
                                            div()
                                                .text_size(px(10.5))
                                                .text_color(theme.text_muted)
                                                .child("Reasoning"),
                                        ),
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
            .absolute()
            .left(px(24.0))
            .right(px(24.0))
            .bottom(px(18.0))
            .mx_auto()
            .max_w(px(920.0))
            .rounded(px(18.0))
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

        crate::frost::frosted(18.0, 16.0, composer).into_any_element()
    }

    fn render_lightbox(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.selected_artifact?;
        let image = self.images.get(&id)?.clone();
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
            .filter_map(|(index, artifact_id)| {
                let thumbnail = self.images.get(artifact_id)?.clone();
                let artifact_id = *artifact_id;
                Some(
                    div()
                        .id(SharedString::from(format!("studio-thumbnail-{index}")))
                        .w(px(if index == selected_index { 64.0 } else { 54.0 }))
                        .h(px(if index == selected_index { 64.0 } else { 54.0 }))
                        .flex_none()
                        .rounded(px(6.0))
                        .overflow_hidden()
                        .border_1()
                        .border_color(if index == selected_index {
                            theme.accent
                        } else {
                            theme.border
                        })
                        .cursor_pointer()
                        .on_click(cx.listener(move |page, _, _, cx| {
                            page.selected_artifact = Some(artifact_id);
                            page.lightbox_zoom = 1.0;
                            page.lightbox_pan = Point::default();
                            if let Some(conversation_id) = page.selected_conversation {
                                cx.emit(StudioEvent::OpenArtifact {
                                    conversation_id,
                                    artifact_id,
                                });
                            }
                            cx.notify();
                        }))
                        .child(img(thumbnail).size_full().object_fit(ObjectFit::Cover)),
                )
            })
            .collect::<Vec<_>>();
        Some(
            div()
                .absolute()
                .inset_0()
                .bg(gpui::black().opacity(0.96))
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
                        "+" | "=" => page.adjust_lightbox_zoom(1.2, cx),
                        "-" => page.adjust_lightbox_zoom(1.0 / 1.2, cx),
                        "0" => page.fit_lightbox(cx),
                        _ => {}
                    }
                }))
                .child(
                    div()
                        .absolute()
                        .top(px(16.0))
                        .left(px(16.0))
                        .id("studio-lightbox-close")
                        .cursor_pointer()
                        .rounded(px(8.0))
                        .px(px(10.0))
                        .py(px(6.0))
                        .bg(crate::theme::ink(0.15))
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.selected_artifact = None;
                            cx.emit(StudioEvent::CloseArtifact);
                            cx.notify();
                        }))
                        .child("Close"),
                )
                .child(
                    div()
                        .id("studio-lightbox-stage")
                        .size_full()
                        .pr(px(320.0))
                        .pb(px(88.0))
                        .overflow_hidden()
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.selected_artifact = None;
                            cx.emit(StudioEvent::CloseArtifact);
                            cx.notify();
                        }))
                        .child(
                            div()
                                .id("studio-lightbox-image")
                                .max_w_full()
                                .max_h_full()
                                .relative()
                                .left(self.lightbox_pan.x)
                                .top(self.lightbox_pan.y)
                                .cursor_pointer()
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(|page, event: &gpui::MouseDownEvent, _, cx| {
                                        cx.stop_propagation();
                                        page.begin_lightbox_pan(event.position, cx);
                                    }),
                                )
                                .on_mouse_move(cx.listener(
                                    |page, event: &gpui::MouseMoveEvent, _, cx| {
                                        if event.dragging() {
                                            page.update_lightbox_pan(event.position, cx);
                                        }
                                    },
                                ))
                                .on_mouse_up(
                                    gpui::MouseButton::Left,
                                    cx.listener(|page, _, _, cx| page.end_lightbox_pan(cx)),
                                )
                                .on_mouse_up_out(
                                    gpui::MouseButton::Left,
                                    cx.listener(|page, _, _, cx| page.end_lightbox_pan(cx)),
                                )
                                .on_click(cx.listener(|page, event: &gpui::ClickEvent, _, cx| {
                                    cx.stop_propagation();
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
                                ),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .top(px(16.0))
                        .left(px(100.0))
                        .flex()
                        .gap(px(6.0))
                        .child(
                            div()
                                .id("studio-zoom-out")
                                .cursor_pointer()
                                .rounded(px(7.0))
                                .px(px(9.0))
                                .py(px(6.0))
                                .bg(crate::theme::ink(0.15))
                                .on_click(cx.listener(|page, _, _, cx| {
                                    page.adjust_lightbox_zoom(1.0 / 1.2, cx)
                                }))
                                .child("−"),
                        )
                        .child(
                            div()
                                .id("studio-zoom-fit")
                                .cursor_pointer()
                                .rounded(px(7.0))
                                .px(px(9.0))
                                .py(px(6.0))
                                .bg(crate::theme::ink(0.15))
                                .on_click(cx.listener(|page, _, _, cx| page.fit_lightbox(cx)))
                                .child(SharedString::from(format!(
                                    "{}%",
                                    (self.lightbox_zoom * 100.0).round() as u32
                                ))),
                        )
                        .child(
                            div()
                                .id("studio-zoom-in")
                                .cursor_pointer()
                                .rounded(px(7.0))
                                .px(px(9.0))
                                .py(px(6.0))
                                .bg(crate::theme::ink(0.15))
                                .on_click(
                                    cx.listener(|page, _, _, cx| {
                                        page.adjust_lightbox_zoom(1.2, cx)
                                    }),
                                )
                                .child("+"),
                        ),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(18.0))
                        .top_1_2()
                        .id("studio-lightbox-previous")
                        .cursor_pointer()
                        .text_size(px(30.0))
                        .on_click(cx.listener(|page, _, _, cx| page.navigate_artifact(-1, cx)))
                        .child("‹"),
                )
                .child(
                    div()
                        .absolute()
                        .right(px(338.0))
                        .top_1_2()
                        .id("studio-lightbox-next")
                        .cursor_pointer()
                        .text_size(px(30.0))
                        .on_click(cx.listener(|page, _, _, cx| page.navigate_artifact(1, cx)))
                        .child("›"),
                )
                .child(
                    div()
                        .absolute()
                        .left(px(16.0))
                        .right(px(336.0))
                        .bottom(px(12.0))
                        .h(px(72.0))
                        .id("studio-lightbox-filmstrip")
                        .overflow_x_scroll()
                        .flex()
                        .items_center()
                        .justify_center()
                        .gap(px(8.0))
                        .children(thumbnails),
                )
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .right_0()
                        .bottom_0()
                        .w(px(320.0))
                        .border_l_1()
                        .border_color(theme.border)
                        .bg(theme.surface)
                        .p(px(20.0))
                        .child(
                            div()
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .child("Artifact inspector"),
                        )
                        .child(
                            div()
                                .mt(px(8.0))
                                .text_size(px(11.0))
                                .text_color(theme.text_faint)
                                .child(SharedString::from(id.0.to_string())),
                        )
                        .when_some(details, |inspector, (prompt, model, mime, size)| {
                            inspector
                                .child(
                                    div()
                                        .mt(px(20.0))
                                        .text_size(px(12.0))
                                        .text_color(theme.text)
                                        .child(SharedString::from(prompt)),
                                )
                                .child(
                                    div()
                                        .mt(px(12.0))
                                        .text_size(px(11.0))
                                        .text_color(theme.text_muted)
                                        .child(SharedString::from(format!(
                                            "{model} · {mime} · {:.1} KB",
                                            size as f64 / 1024.0
                                        ))),
                                )
                        })
                        .child(
                            div()
                                .id("studio-delete-artifact")
                                .mt(px(20.0))
                                .cursor_pointer()
                                .rounded(px(7.0))
                                .border_1()
                                .border_color(theme.danger)
                                .text_color(theme.danger)
                                .px(px(10.0))
                                .py(px(6.0))
                                .on_click(
                                    cx.listener(move |page, _, _, cx| page.delete_artifact(id, cx)),
                                )
                                .child("Delete artifact"),
                        )
                        .child(
                            div()
                                .id("studio-download-artifact")
                                .mt(px(8.0))
                                .cursor_pointer()
                                .rounded(px(7.0))
                                .border_1()
                                .border_color(theme.border)
                                .bg(theme.surface)
                                .text_color(theme.text)
                                .px(px(10.0))
                                .py(px(6.0))
                                .on_click(
                                    cx.listener(move |page, _, _, cx| {
                                        page.download_artifact(id, cx)
                                    }),
                                )
                                .child("Download"),
                        ),
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
        let theme = Theme::of(cx).clone();
        let body = if self.providers.iter().all(|provider| !provider.configured) {
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
                        .when(has_turns, |feed| feed.px(px(24.0)).pt(px(22.0)))
                        .child(self.render_feed(window, &theme, cx)),
                )
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
            .when_some(self.render_lightbox(&theme, cx), |el, lightbox| {
                el.child(lightbox)
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
                    Ok(value) => page.providers = value.providers,
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
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
}

impl Render for ProvidersPage {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = Theme::of(cx).clone();
        let cards = self
            .providers
            .clone()
            .into_iter()
            .map(|provider| {
                let id = provider.provider_id.clone();
                let remove_id = provider.provider_id.clone();
                let configured = provider.configured;
                let status = match provider.validation_state {
                    ProviderValidationState::Valid => "Connected",
                    ProviderValidationState::Invalid => "Invalid key",
                    ProviderValidationState::Unavailable => "Provider unavailable",
                    ProviderValidationState::NotValidated if provider.configured => "Not validated",
                    _ => "Not connected",
                };
                div()
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.border)
                    .p(px(16.0))
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .justify_between()
                            .child(
                                div()
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .child(SharedString::from(provider.display_label)),
                            )
                            .child(
                                div()
                                    .text_size(px(11.0))
                                    .text_color(
                                        if provider.validation_state
                                            == ProviderValidationState::Valid
                                        {
                                            theme.success
                                        } else {
                                            theme.text_muted
                                        },
                                    )
                                    .child(status),
                            ),
                    )
                    .child(
                        div()
                            .rounded(px(8.0))
                            .border_1()
                            .border_color(theme.border)
                            .px(px(10.0))
                            .py(px(7.0))
                            .child(self.secret.clone()),
                    )
                    .child(
                        div()
                            .flex()
                            .self_end()
                            .gap(px(8.0))
                            .when(configured, |row| {
                                row.child(
                                    div()
                                        .id(SharedString::from(format!(
                                            "provider-remove-{}",
                                            remove_id.as_str()
                                        )))
                                        .cursor_pointer()
                                        .rounded(px(7.0))
                                        .border_1()
                                        .border_color(theme.border)
                                        .px(px(12.0))
                                        .py(px(6.0))
                                        .on_click(cx.listener(move |page, _, _, cx| {
                                            page.remove(remove_id.clone(), cx)
                                        }))
                                        .child("Remove"),
                                )
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!(
                                        "provider-save-{}",
                                        id.as_str()
                                    )))
                                    .cursor_pointer()
                                    .rounded(px(8.0))
                                    .border_1()
                                    .border_color(theme.border)
                                    .bg(crate::theme::ink(0.02))
                                    .text_color(theme.text)
                                    .px(px(12.0))
                                    .py(px(6.0))
                                    .hover(|style| {
                                        style
                                            .bg(crate::theme::ink(0.05))
                                            .border_color(theme.border_strong)
                                    })
                                    .on_click(
                                        cx.listener(move |page, _, _, cx| {
                                            page.save(id.clone(), cx)
                                        }),
                                    )
                                    .child(if self.loading {
                                        "Working…"
                                    } else {
                                        "Save and validate"
                                    }),
                            ),
                    )
            })
            .collect::<Vec<_>>();
        div().id("providers-scroll").size_full().overflow_y_scroll().child(div().w_full().max_w(px(768.0)).mx_auto().px(px(24.0)).pt(px(32.0)).pb(px(64.0)).flex().flex_col().gap(px(16.0))
            .child(div().text_size(px(16.0)).font_weight(gpui::FontWeight::SEMIBOLD).child("Providers"))
            .child(div().text_size(px(13.0)).text_color(theme.text_muted).child("Credentials stay in this device’s platform secret store and are never synced."))
            .children(cards)
            .when_some(self.error.clone(), |el, error| el.child(div().text_color(theme.danger).child(error))))
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
}
