//! Studio conversation page: load, submit, and the feed/lightbox outlet.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui::{
    Bounds, Context, DragMoveEvent, Entity, EventEmitter, ExternalPaths, FocusHandle, Focusable,
    Image, Pixels, Point, Render, SharedString, Subscription, Task, Window, div, prelude::*, px,
};
use zeron_proto::{
    ListStudioConversationsResponse, ListStudioModelsResponse, ListStudioProvidersResponse,
    QuoteStudioBatchResponse, StudioConversationSummary, StudioConversationView, StudioGalleryItem,
    StudioProviderBalanceResponse, StudioProviderConnection, StudioRunState, StudioRunView,
    StudioTurnView, UNTITLED_STUDIO_TITLE,
};
use zeron_rpc::methods;
use zeron_studio::{
    ComposerEvent, ComposerMode, ComposerSnapshot, ComposerView, ConflictId, MediaOperation,
    StudioArtifactId, StudioAssetId, StudioConversationId, evaluate_composer,
};

use crate::composer::{PromptHistory, PromptHistoryItem};
use crate::popover;
use crate::state::{AppState, EngineHandle};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme::Theme;

use super::StudioEvent;
use super::artifact::{render_run_error_chip, run_error_message};
use super::defaults::StudioDefaults;
use super::draft::{
    DraftRunConfig, apply_remembered_drafts, draft_aspect, snapshot_from_committed_turn,
};
use super::feed::{
    ARTIFACT_FOCUS_FADE, ARTIFACT_FOCUS_HOLD, FeedLayoutSig, artifact_focus_alpha,
    conversation_image_count, new_feed_list,
};
use super::gallery::new_gallery_list;
use super::image_menu::ImageMenu;
use super::images::StudioImages;
use super::lineage::{LineageIndex, LineageKey};
use super::upscale::UpscaleJob;

pub struct StudioPage {
    pub(super) state: Entity<AppState>,
    pub(super) conversations: Vec<StudioConversationSummary>,
    pub(super) providers: Vec<StudioProviderConnection>,
    pub(super) models: Vec<zeron_studio::MediaModel>,
    pub(super) upscale_models: Vec<zeron_studio::MediaModel>,
    pub(super) edit_models: Vec<zeron_studio::MediaModel>,
    pub(super) selected_models: BTreeSet<zeron_studio::ModelId>,
    pub(super) draft_runs: HashMap<zeron_studio::ModelId, DraftRunConfig>,
    pub(super) composer: ComposerSnapshot,
    pub(super) composer_view: ComposerView,
    pub(super) popup_conflict: Option<ConflictId>,
    pub(super) conflict_more_open: bool,
    /// Scroll handle for the (shared) composer tray's row list.
    pub(super) tray_scroll: gpui::ScrollHandle,
    pub(super) catalog_refresh_task: Option<Task<()>>,
    pub(super) import_tasks: HashMap<StudioAssetId, Task<()>>,
    pub(super) tray_picker_task: Option<Task<()>>,
    pub(super) tray_previews: HashMap<StudioAssetId, Arc<Image>>,
    pub(super) remembered: StudioDefaults,
    pub(super) live_quotes: HashMap<zeron_studio::ModelId, zeron_studio::Quote>,
    pub(super) quote_generation: u64,
    pub(super) quote_task: Option<Task<()>>,
    pub(super) account_balance: Option<zeron_studio::AccountBalance>,
    pub(super) reserved_spend: Option<zeron_studio::Quote>,
    pub(super) balance_generation: u64,
    pub(super) balance_task: Option<Task<()>>,
    pub(super) composer_seeded_for: Option<StudioConversationId>,
    pub(super) selected_conversation: Option<StudioConversationId>,
    pub(super) conversation: Option<StudioConversationView>,
    pub(super) prompt: Entity<TextInput>,
    /// Up/Down overflow through this conversation's turn prompts. Reset on
    /// submit and conversation switch; the in-progress draft is the scratch.
    pub(super) prompt_history: PromptHistory,
    pub(super) prompt_expanded: bool,
    pub(super) prompt_morph: Option<crate::composer::FlipMorph>,
    pub(super) prompt_last_height: f32,
    pub(super) prompt_target_height: f32,
    pub(super) prompt_morph_clock: Instant,
    pub(super) model_search: Entity<TextInput>,
    pub(super) model_picker: popover::Popup<()>,
    pub(super) model_picker_active: Option<usize>,
    pub(super) model_picker_scroll: gpui::ScrollHandle,
    pub(super) model_picker_focus: FocusHandle,
    pub(super) model_picker_favorites: bool,
    pub(super) model_picker_filters: BTreeSet<zeron_studio::ModelFeature>,
    pub(super) model_picker_operations: BTreeSet<zeron_studio::MediaOperation>,
    pub(super) model_chips_scroll: gpui::ScrollHandle,
    pub(super) file_drag_active: bool,
    pub(super) tray_preview: Option<crate::attachments::PreviewImage>,
    pub(super) tray_preview_focus: FocusHandle,
    pub(super) tray_preview_focus_pending: bool,
    pub(super) model_config_menu: popover::Popup<zeron_studio::ModelId>,
    pub(super) duration_popup: popover::Popup<()>,
    pub(super) duration_dragging: bool,
    pub(super) duration_drag_from_chip: bool,
    pub(super) duration_drag_origin_x: f32,
    pub(super) duration_drag_start_index: usize,
    pub(super) duration_drag_moved: bool,
    pub(super) duration_track: Option<Bounds<Pixels>>,
    pub(super) feed_list: gpui::ListState,
    pub(super) feed_width: f32,
    pub(super) feed_columns: usize,
    pub(super) feed_visible_rows: Range<usize>,
    /// Thread tiles whose originals should become 1280px display frames.
    pub(super) feed_viewport_fulls: HashSet<StudioArtifactId>,
    pub(super) feed_layout_sig: Option<FeedLayoutSig>,
    /// One walk of the open conversation. Rebuilt when the watch snapshot
    /// changes, never on scroll.
    pub(super) lineage: LineageIndex,
    pub(super) lineage_key: Option<LineageKey>,
    /// Last list-viewport band used to skip off-screen tiles inside a turn.
    pub(super) feed_cull_top: f32,
    pub(super) feed_cull_bottom: f32,
    pub(super) feed_item_tops: Vec<Option<f32>>,
    pub(super) scroll_after_turn_count: Option<usize>,
    pub(super) scroll_after_extend: Option<zeron_studio::StudioTurnId>,
    pub(super) scroll_to_artifact: Option<StudioArtifactId>,
    pub(super) focused_artifact: Option<(StudioArtifactId, Instant)>,
    pub(super) focus_task: Option<Task<()>>,
    pub(super) scroll_task: Option<Task<()>>,
    pub(super) rail_hover: Option<usize>,
    pub(super) source_turn: Option<zeron_studio::StudioTurnId>,
    /// Turns whose prompt bubble is fully expanded past the 3-line clamp.
    pub(super) expanded_prompts: HashSet<zeron_studio::StudioTurnId>,
    /// Prompt copy that is showing the check flash.
    pub(super) copied_prompt: Option<zeron_studio::StudioTurnId>,
    pub(super) copied_prompt_clear: Option<Task<()>>,
    /// Artifact image copy that is showing the check flash.
    pub(super) copied_artifact: Option<StudioArtifactId>,
    pub(super) copied_artifact_clear: Option<Task<()>>,
    /// Turns whose inspector prompt is fully expanded past the 10-line clamp.
    pub(super) expanded_inspector_prompts: HashSet<zeron_studio::StudioTurnId>,
    pub(super) inspector_scroll: gpui::ScrollHandle,
    pub(super) images: StudioImages,
    pub(super) loading_images: HashSet<StudioArtifactId>,
    /// In-flight original reads for hover/lightbox. Independent of preview reads.
    pub(super) loading_full_images: HashSet<StudioArtifactId>,
    pub(super) image_failed: HashSet<StudioArtifactId>,
    pub(super) preview_failed: HashSet<StudioArtifactId>,
    pub(super) image_protect: HashSet<StudioArtifactId>,
    pub(super) gallery: Vec<StudioGalleryItem>,
    pub(super) gallery_list: gpui::ListState,
    pub(super) gallery_width: f32,
    pub(super) gallery_list_columns: usize,
    pub(super) gallery_row_px: f32,
    pub(super) gallery_visible_rows: std::ops::Range<usize>,
    pub(super) gallery_selected: BTreeSet<StudioArtifactId>,
    pub(super) gallery_anchor: Option<StudioArtifactId>,
    pub(super) image_menu: popover::Popup<ImageMenu>,
    pub(super) upscale_settings_menu: popover::Popup<StudioArtifactId>,
    pub(super) artifact_actions_menu: popover::Popup<StudioArtifactId>,
    pub(super) upscale_jobs: HashMap<StudioArtifactId, UpscaleJob>,
    pub(super) upscale_watch_tasks: HashMap<StudioConversationId, Task<()>>,
    pub(super) selected_frame: Option<super::artifact::ArtifactFrameKey>,
    pub(super) lightbox_frames: Vec<super::artifact::ArtifactFrame>,
    pub(super) compare_pressed: bool,
    pub(super) lightbox_zoom: f32,
    pub(super) lightbox_pan: Point<Pixels>,
    pub(super) lightbox_drag: Option<Point<Pixels>>,
    pub(super) lightbox_zoom_spring: Option<super::artifact::LightboxZoomSpring>,
    pub(super) lightbox_zoom_last_tick: Option<Instant>,
    pub(super) lightbox_swipe_x: f32,
    pub(super) lightbox_swipe_velocity: f32,
    pub(super) lightbox_snap: Option<super::artifact::LightboxSnap>,
    pub(super) lightbox_swipe_scheduled: bool,
    pub(super) lightbox_swipe_last_tick: Option<Instant>,
    pub(super) lightbox_swipe_locked: bool,
    pub(super) lightbox_stage_width: f32,
    pub(super) lightbox_stage_height: f32,
    pub(super) lightbox_stage_origin: Point<Pixels>,
    pub(super) filmstrip_scroll_accum: f32,
    pub(super) edit_target: Option<StudioArtifactId>,
    pub(super) edit_prompt: Entity<TextInput>,
    pub(super) edit_paint: Option<super::paint::PaintSession>,
    pub(super) edit_brush_t: f32,
    pub(super) edit_brush_drag: bool,
    pub(super) edit_brush_track: Option<Bounds<Pixels>>,
    pub(super) edit_brush_preview: bool,
    pub(super) edit_brush_preview_clear: Option<Task<()>>,
    pub(super) edit_add: bool,
    pub(super) edit_model_picker: popover::Popup<()>,
    pub(super) edit_model_picker_active: Option<usize>,
    pub(super) edit_model_picker_scroll: gpui::ScrollHandle,
    pub(super) edit_model_picker_focus: FocusHandle,
    pub(super) edit_space_pan: bool,
    pub(super) pending_edit_source: Option<StudioArtifactId>,
    pub(super) focus: FocusHandle,
    pub(super) loading: bool,
    pub(super) busy: bool,
    pub(super) error: Option<SharedString>,
    pub(super) load_task: Option<Task<()>>,
    pub(super) watch_task: Option<Task<()>>,
    pub(super) conversations_watch_task: Option<Task<()>>,
    pub(super) gallery_watch_task: Option<Task<()>>,
    pub(super) action_task: Option<Task<()>>,
    pub(super) image_tasks: HashMap<StudioArtifactId, Task<()>>,
    pub(super) full_image_tasks: HashMap<StudioArtifactId, Task<()>>,
    pub(super) display_tasks: HashMap<StudioArtifactId, Task<()>>,
    pub(super) loading_displays: HashSet<StudioArtifactId>,
    /// Most recent visible paint of an in-flight output. A ready image may
    /// reveal only when it immediately replaces that paint, never on remount.
    pub(super) visible_loading_tiles: HashMap<(zeron_studio::StudioRunId, usize), Instant>,
    pub(super) video: Option<super::video::StudioVideoPlayback>,
    pub(super) video_task: Option<Task<()>>,
    pub(super) video_frame_scheduled: bool,
    /// Tile currently armed for muted hover autoplay. Distinct from lightbox.
    pub(super) hover_target: Option<StudioArtifactId>,
    pub(super) hover_generation: u64,
    pub(super) hover_play: Option<super::video::StudioVideoPlayback>,
    pub(super) hover_task: Option<Task<()>>,
    pub(super) _observe: Subscription,
    pub(super) _prompt_events: Subscription,
    pub(super) _edit_prompt_events: Subscription,
    pub(super) _model_search_events: Subscription,
}

impl StudioPage {
    pub fn new(state: Entity<AppState>, cx: &mut Context<Self>) -> Self {
        let observe = cx.observe(&state, |_, _, cx| cx.notify());
        let prompt = cx.new(|cx| TextInput::composer("Describe the image you want to create", cx));
        let edit_prompt = cx.new(|cx| {
            let mut input = TextInput::new("Describe the edit…", cx);
            input.set_soft_wrap(false);
            input.set_viewport_max(Some(crate::text_input::INPUT_LINE_HEIGHT));
            input
        });
        let model_search = cx.new(|cx| TextInput::palette_search("Search models…", cx));
        let prompt_events = cx.subscribe(&prompt, |page: &mut Self, _, event, cx| match event {
            TextInputEvent::Submitted => page.submit(cx),
            TextInputEvent::Edited => page.on_prompt_edited(cx),
            TextInputEvent::HistoryNavigate(dir) => page.on_history_navigate(*dir, cx),
            TextInputEvent::PastedImages(images) => page.add_pasted_images(images.clone(), cx),
            TextInputEvent::PastedPaths(paths) => page.add_dropped_paths(paths.clone(), cx),
            _ => {}
        });
        let edit_prompt_events =
            cx.subscribe(&edit_prompt, |page: &mut Self, _, event, cx| match event {
                TextInputEvent::Submitted => page.submit_edit(cx),
                TextInputEvent::Edited => cx.notify(),
                _ => {}
            });
        let model_search_events =
            cx.subscribe(&model_search, |page: &mut Self, _, event, cx| match event {
                TextInputEvent::Edited => {
                    page.model_picker_active = None;
                    page.model_picker_scroll.set_offset(Point::default());
                    cx.notify();
                }
                TextInputEvent::Submitted => page.activate_model_picker_row(cx),
                _ => {}
            });
        let remembered = state
            .read(cx)
            .data_dir
            .as_deref()
            .map(StudioDefaults::load)
            .unwrap_or_default();
        let mut page = Self {
            state,
            conversations: Vec::new(),
            providers: Vec::new(),
            models: Vec::new(),
            upscale_models: Vec::new(),
            edit_models: Vec::new(),
            selected_models: BTreeSet::new(),
            draft_runs: HashMap::new(),
            composer: ComposerSnapshot::default(),
            composer_view: evaluate_composer(&ComposerSnapshot::default(), &[]),
            popup_conflict: None,
            conflict_more_open: false,
            tray_scroll: gpui::ScrollHandle::new(),
            catalog_refresh_task: None,
            import_tasks: HashMap::new(),
            tray_picker_task: None,
            tray_previews: HashMap::new(),
            remembered,
            live_quotes: HashMap::new(),
            quote_generation: 0,
            quote_task: None,
            account_balance: None,
            reserved_spend: None,
            balance_generation: 0,
            balance_task: None,
            composer_seeded_for: None,
            selected_conversation: None,
            conversation: None,
            prompt,
            edit_prompt,
            prompt_history: PromptHistory::default(),
            prompt_expanded: false,
            prompt_morph: None,
            prompt_last_height: 0.0,
            prompt_target_height: 0.0,
            prompt_morph_clock: Instant::now(),
            model_search,
            model_picker: popover::Popup::default(),
            model_picker_active: None,
            model_picker_scroll: gpui::ScrollHandle::new(),
            model_picker_focus: cx.focus_handle(),
            model_picker_favorites: false,
            model_picker_filters: BTreeSet::new(),
            model_picker_operations: BTreeSet::new(),
            model_chips_scroll: gpui::ScrollHandle::new(),
            file_drag_active: false,
            tray_preview: None,
            tray_preview_focus: cx.focus_handle(),
            tray_preview_focus_pending: false,
            model_config_menu: popover::Popup::default(),
            duration_popup: popover::Popup::default(),
            duration_dragging: false,
            duration_drag_from_chip: false,
            duration_drag_origin_x: 0.0,
            duration_drag_start_index: 0,
            duration_drag_moved: false,
            duration_track: None,
            feed_list: new_feed_list(cx),
            feed_width: 0.0,
            feed_columns: 0,
            feed_visible_rows: 0..0,
            feed_viewport_fulls: HashSet::new(),
            feed_layout_sig: None,
            lineage: LineageIndex::default(),
            lineage_key: None,
            feed_cull_top: 0.0,
            feed_cull_bottom: 0.0,
            feed_item_tops: Vec::new(),
            scroll_after_turn_count: None,
            scroll_after_extend: None,
            scroll_to_artifact: None,
            focused_artifact: None,
            focus_task: None,
            scroll_task: None,
            rail_hover: None,
            source_turn: None,
            expanded_prompts: HashSet::new(),
            copied_prompt: None,
            copied_prompt_clear: None,
            copied_artifact: None,
            copied_artifact_clear: None,
            expanded_inspector_prompts: HashSet::new(),
            inspector_scroll: gpui::ScrollHandle::new(),
            images: StudioImages::default(),
            loading_images: HashSet::new(),
            loading_full_images: HashSet::new(),
            image_failed: HashSet::new(),
            image_protect: HashSet::new(),
            gallery: Vec::new(),
            gallery_list: new_gallery_list(cx),
            gallery_width: 0.0,
            gallery_list_columns: 0,
            gallery_row_px: 0.0,
            gallery_visible_rows: 0..0,
            gallery_selected: BTreeSet::new(),
            gallery_anchor: None,
            image_menu: popover::Popup::default(),
            upscale_settings_menu: popover::Popup::default(),
            artifact_actions_menu: popover::Popup::default(),
            upscale_jobs: HashMap::new(),
            upscale_watch_tasks: HashMap::new(),
            selected_frame: None,
            lightbox_frames: Vec::new(),
            compare_pressed: false,
            lightbox_zoom: 1.0,
            lightbox_pan: Point::default(),
            lightbox_drag: None,
            lightbox_zoom_spring: None,
            lightbox_zoom_last_tick: None,
            lightbox_swipe_x: 0.0,
            lightbox_swipe_velocity: 0.0,
            lightbox_snap: None,
            lightbox_swipe_scheduled: false,
            lightbox_swipe_last_tick: None,
            lightbox_swipe_locked: false,
            lightbox_stage_width: 0.0,
            lightbox_stage_height: 0.0,
            lightbox_stage_origin: Point::default(),
            filmstrip_scroll_accum: 0.0,
            edit_target: None,
            edit_paint: None,
            edit_brush_t: 0.28,
            edit_brush_drag: false,
            edit_brush_track: None,
            edit_brush_preview: false,
            edit_brush_preview_clear: None,
            edit_add: true,
            edit_model_picker: popover::Popup::default(),
            edit_model_picker_active: None,
            edit_model_picker_scroll: gpui::ScrollHandle::new(),
            edit_model_picker_focus: cx.focus_handle(),
            edit_space_pan: false,
            pending_edit_source: None,
            focus: cx.focus_handle(),
            loading: false,
            busy: false,
            error: None,
            load_task: None,
            watch_task: None,
            conversations_watch_task: None,
            gallery_watch_task: None,
            action_task: None,
            image_tasks: HashMap::new(),
            full_image_tasks: HashMap::new(),
            display_tasks: HashMap::new(),
            loading_displays: HashSet::new(),
            visible_loading_tiles: HashMap::new(),
            preview_failed: HashSet::new(),
            video: None,
            video_task: None,
            video_frame_scheduled: false,
            hover_target: None,
            hover_generation: 0,
            hover_play: None,
            hover_task: None,
            _observe: observe,
            _prompt_events: prompt_events,
            _edit_prompt_events: edit_prompt_events,
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
                && let Ok(list) =
                    serde_json::from_value::<ListStudioProvidersResponse>(value.clone())
                && let Some(provider) = list.providers.iter().find(|provider| provider.configured)
            {
                models = engine
                    .client()
                    .call(
                        methods::LIST_STUDIO_MODELS,
                        serde_json::json!({ "providerId": provider.provider_id }),
                    )
                    .await
                    .map_err(|error| error.to_string())
                    .and_then(|value| {
                        serde_json::from_value(value).map_err(|error| error.to_string())
                    });
            }
            this.update(cx, |page, cx| {
                page.loading = false;
                match (providers, conversations, models) {
                    (Ok(providers), Ok(conversations), Ok(models)) => {
                        page.providers =
                            serde_json::from_value::<ListStudioProvidersResponse>(providers)
                                .map(|value| value.providers)
                                .unwrap_or_default();
                        page.conversations = serde_json::from_value::<
                            ListStudioConversationsResponse,
                        >(conversations)
                        .map(|value| value.conversations)
                        .unwrap_or_default();
                        page.apply_models(models.models);
                        page.apply_remembered_or_default_models(cx);
                        page.refresh_account_balance(cx, false);
                        page.watch_conversations(cx);
                        page.watch_gallery(cx);
                        cx.emit(StudioEvent::SidebarChanged);
                    }
                    (Err(error), _, _) | (_, Err(error), _) => {
                        page.error = Some(error.to_string().into())
                    }
                    (_, _, Err(error)) => page.error = Some(error.into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn conversations(&self) -> &[StudioConversationSummary] {
        &self.conversations
    }

    pub fn selected_conversation(&self) -> Option<StudioConversationId> {
        self.selected_conversation
    }

    pub fn conversation_title(&self, id: StudioConversationId) -> Option<String> {
        self.conversations
            .iter()
            .find(|item| item.id == id)
            .map(|item| item.title.clone())
    }

    /// Title of the open conversation, for the window titlebar.
    pub fn selected_title(&self) -> Option<String> {
        self.selected_conversation
            .and_then(|id| self.conversation_title(id))
            .or_else(|| Some("Gallery".into()))
    }

    /// Prepaid USD remaining after in-flight draft reservations.
    pub fn account_balance_label(&self) -> Option<String> {
        let remaining = super::cost::remaining_after_spend(
            &self.account_balance.as_ref()?.remaining,
            self.reserved_spend.as_ref(),
        );
        Some(super::cost::format_quote(&remaining))
    }

    /// Images currently on the open conversation, or the gallery when no
    /// conversation is selected. `None` until the conversation view arrives,
    /// so the titlebar does not flash "0 images" on a switch.
    pub fn selected_image_count(&self) -> Option<u32> {
        if self.selected_conversation.is_some() {
            self.conversation
                .as_ref()
                .map(|view| conversation_image_count(&view.turns))
        } else {
            Some(self.gallery_image_count())
        }
    }

    fn apply_conversation_summary(
        &mut self,
        summary: StudioConversationSummary,
        cx: &mut Context<Self>,
    ) {
        let changed = if summary.archived {
            let before = self.conversations.len();
            self.conversations.retain(|item| item.id != summary.id);
            before != self.conversations.len()
        } else if let Some(existing) = self
            .conversations
            .iter_mut()
            .find(|item| item.id == summary.id)
        {
            if *existing == summary {
                false
            } else {
                *existing = summary.clone();
                true
            }
        } else {
            self.conversations.push(summary.clone());
            true
        };
        if changed {
            self.conversations.sort_by(|left, right| {
                right
                    .updated_at
                    .cmp(&left.updated_at)
                    .then_with(|| right.id.0.cmp(&left.id.0))
            });
            cx.emit(StudioEvent::SidebarChanged);
        }
        if let Some(view) = self.conversation.as_mut()
            && view.conversation.id == summary.id
        {
            view.conversation = summary.clone();
        }
        if self.selected_conversation == Some(summary.id) && summary.done {
            self.mark_conversation_seen(summary.id, cx);
        }
    }

    fn apply_conversation_list(
        &mut self,
        conversations: Vec<StudioConversationSummary>,
        cx: &mut Context<Self>,
    ) {
        if self.conversations == conversations {
            return;
        }
        self.conversations = conversations;
        if let Some(view) = self.conversation.as_mut()
            && let Some(summary) = self
                .conversations
                .iter()
                .find(|item| item.id == view.conversation.id)
        {
            view.conversation = summary.clone();
        }
        cx.emit(StudioEvent::SidebarChanged);
        if let Some(id) = self.selected_conversation
            && self
                .conversations
                .iter()
                .any(|item| item.id == id && item.done)
        {
            self.mark_conversation_seen(id, cx);
        }
    }

    pub fn selected_unseen(&self) -> Option<StudioConversationId> {
        let id = self.selected_conversation?;
        self.conversations
            .iter()
            .find(|item| item.id == id && item.done)
            .map(|item| item.id)
    }

    pub fn mark_conversation_seen(&mut self, id: StudioConversationId, cx: &mut Context<Self>) {
        let Some(item) = self.conversations.iter_mut().find(|item| item.id == id) else {
            return;
        };
        if !item.done {
            return;
        }
        item.done = false;
        if let Some(view) = self.conversation.as_mut()
            && view.conversation.id == id
        {
            view.conversation.done = false;
        }
        cx.emit(StudioEvent::SidebarChanged);
        let Some(engine) = self.engine(cx) else {
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::MARK_STUDIO_CONVERSATION_SEEN,
                    serde_json::json!({ "conversationId": id }),
                )
                .await;
            this.update(cx, |page, cx| {
                if let Ok(value) = result
                    && let Ok(summary) = serde_json::from_value::<StudioConversationSummary>(value)
                {
                    page.apply_conversation_summary(summary, cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn watch_conversations(&mut self, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.conversations_watch_task = Some(cx.spawn(async move |this, cx| {
            let stream = engine
                .client()
                .subscribe(methods::WATCH_STUDIO_CONVERSATIONS, serde_json::json!({}))
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
                let Ok(list) = serde_json::from_value::<ListStudioConversationsResponse>(value)
                else {
                    continue;
                };
                if this
                    .update(cx, |page, cx| {
                        page.apply_conversation_list(list.conversations, cx);
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
            }
        }));
    }

    pub fn open_conversation(&mut self, id: StudioConversationId, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.stop_hover_playback();
        self.close_image_menu(cx);
        if self.selected_conversation != Some(id) {
            self.focused_artifact = None;
            self.close_artifact(cx);
            self.composer_seeded_for = None;
            self.source_turn = None;
            self.prompt_history.reset();
            self.expanded_prompts.clear();
            self.expanded_inspector_prompts.clear();
            self.inspector_scroll.set_offset(Point::default());
        }
        self.selected_conversation = Some(id);
        self.conversation = None;
        self.scroll_after_turn_count = None;
        self.scroll_after_extend = None;
        self.scroll_task = None;
        // Keep a pending reveal across the reload so Open thread can land
        // on the image once the watch snapshot arrives.
        self.rail_hover = None;
        self.reset_feed_list();
        self.lineage = LineageIndex::default();
        self.lineage_key = None;
        cx.emit(StudioEvent::SidebarChanged);
        self.mark_conversation_seen(id, cx);
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
                        let first_open = page.conversation.is_none();
                        let settled = page
                            .conversation
                            .as_ref()
                            .is_some_and(|previous| newly_settled_runs(previous, &view));
                        let submitted_turn_arrived = page
                            .scroll_after_turn_count
                            .is_some_and(|before| view.turns.len() > before);
                        let last_turn_id = view.turns.last().map(|turn| turn.id);
                        if let Some(previous) = page.conversation.as_ref()
                            && let Some(message) = first_newly_failed_message(previous, &view)
                        {
                            page.error = Some(message);
                        }
                        page.observe_upscale_view(&view, cx);
                        page.apply_conversation_summary(view.conversation.clone(), cx);
                        page.conversation = Some(view.clone());
                        page.sync_lineage();
                        if let Some(source_id) = page.pending_edit_source {
                            page.select_pending_derived(&view, source_id, cx);
                        } else if page.selected_frame.is_some() {
                            page.refresh_lightbox_frames(cx);
                        }
                        page.snap_prompt_history(cx);
                        if settled {
                            page.refresh_account_balance(cx, true);
                        }
                        page.seed_composer_from_conversation(cx);
                        page.sync_feed_list();
                        if page.apply_scroll_to_artifact(cx) {
                            page.scroll_after_turn_count = None;
                        } else if first_open || submitted_turn_arrived {
                            page.scroll_after_turn_count = None;
                            page.feed_scroll_to_end();
                        } else if page
                            .scroll_after_extend
                            .zip(last_turn_id)
                            .is_some_and(|(wanted, last)| wanted == last)
                        {
                            page.scroll_after_extend = None;
                            page.feed_scroll_to_end();
                        }
                        page.request_visible_feed_images(cx);
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

    pub fn reveal_artifact_in_thread(
        &mut self,
        artifact_id: StudioArtifactId,
        cx: &mut Context<Self>,
    ) {
        self.scroll_to_artifact = Some(artifact_id);
        self.apply_scroll_to_artifact(cx);
    }

    fn apply_scroll_to_artifact(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(artifact_id) = self.scroll_to_artifact else {
            return false;
        };
        if self.conversation.is_none() {
            return false;
        }
        self.sync_lineage();
        let (content_width, tile_width, gap, columns) = self.feed_target_metrics();
        let expanded = self
            .lineage
            .tiles()
            .iter()
            .find(|tile| tile.artifact_id == Some(artifact_id))
            .is_some_and(|tile| self.expanded_prompts.contains(&tile.root_turn_id));
        let Some(view) = self.conversation.as_ref() else {
            return false;
        };
        let target = super::feed::artifact_feed_target_in_lineage(
            &self.lineage,
            &view.turns,
            artifact_id,
            content_width,
            tile_width,
            gap,
            columns,
            expanded,
        );
        self.scroll_to_artifact = None;
        match target {
            Some(target) => {
                self.focus_artifact_tile(artifact_id, cx);
                self.scroll_to_turn_offset(target.turn_ix, target.offset, cx);
            }
            None => self.feed_scroll_to_end(),
        }
        true
    }

    fn feed_target_metrics(&self) -> (f32, f32, f32, usize) {
        let width = if self.feed_width > 1.0 {
            self.feed_width
        } else if self.gallery_width > 1.0 {
            self.gallery_width
        } else {
            960.0
        };
        let rail_gutter = if self.rail_should_show(width) {
            super::feed::STUDIO_RAIL_GUTTER
        } else {
            0.0
        };
        let available = (width - super::feed::FEED_PAD_X * 2.0 - rail_gutter).clamp(240.0, 1600.0);
        let columns = super::feed::grid_columns_sticky(available, self.feed_columns);
        let gap = if available < 520.0 { 12.0 } else { 16.0 };
        let tile = (available - gap * (columns.saturating_sub(1) as f32)) / columns.max(1) as f32;
        (available, tile, gap, columns)
    }

    fn focus_artifact_tile(&mut self, artifact_id: StudioArtifactId, cx: &mut Context<Self>) {
        self.focused_artifact = Some((artifact_id, Instant::now()));
        let reduced = crate::motion::reduced_motion(cx);
        self.focus_task = Some(cx.spawn(async move |this, cx| {
            let hold = if reduced {
                ARTIFACT_FOCUS_HOLD + ARTIFACT_FOCUS_FADE
            } else {
                ARTIFACT_FOCUS_HOLD
            };
            cx.background_executor().timer(hold).await;
            if !reduced {
                let fade_started = Instant::now();
                while fade_started.elapsed() < ARTIFACT_FOCUS_FADE {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        return;
                    }
                    cx.background_executor()
                        .timer(Duration::from_millis(16))
                        .await;
                }
            }
            this.update(cx, |page, cx| {
                if page
                    .focused_artifact
                    .is_some_and(|(id, _)| id == artifact_id)
                {
                    page.focused_artifact = None;
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn artifact_focus_alpha(&self, artifact_id: StudioArtifactId) -> Option<f32> {
        let (id, since) = self.focused_artifact?;
        if id != artifact_id {
            return None;
        }
        artifact_focus_alpha(since.elapsed())
    }

    pub(super) fn forget_artifact(&mut self, artifact_id: StudioArtifactId) {
        self.images.remove(&artifact_id);
        self.loading_images.remove(&artifact_id);
        self.loading_full_images.remove(&artifact_id);
        self.loading_displays.remove(&artifact_id);
        self.image_failed.remove(&artifact_id);
        self.preview_failed.remove(&artifact_id);
        self.image_tasks.remove(&artifact_id);
        self.full_image_tasks.remove(&artifact_id);
        self.display_tasks.remove(&artifact_id);
        self.feed_viewport_fulls.remove(&artifact_id);
        if self
            .video
            .as_ref()
            .is_some_and(|player| player.artifact_id == artifact_id)
        {
            self.stop_video_playback();
        }
        if self.hover_target == Some(artifact_id)
            || self
                .hover_play
                .as_ref()
                .is_some_and(|player| player.artifact_id == artifact_id)
        {
            self.stop_hover_playback();
        }
        self.gallery.retain(|item| item.id != artifact_id);
        self.gallery_selected.remove(&artifact_id);
        if self.gallery_anchor == Some(artifact_id) {
            self.gallery_anchor = None;
        }
        self.lightbox_frames
            .retain(|frame| frame.artifact_id() != Some(artifact_id));
        if self.selected_artifact_id() == Some(artifact_id) {
            self.selected_frame = None;
            self.reset_lightbox_viewer();
        }
        if let Some(view) = self.conversation.as_mut() {
            for turn in &mut view.turns {
                for run in &mut turn.runs {
                    run.artifacts.retain(|artifact| artifact.id != artifact_id);
                }
            }
        }
        self.sync_gallery_list(self.gallery_width.max(1.0));
        self.sync_feed_list();
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
                    serde_json::json!({ "title": UNTITLED_STUDIO_TITLE }),
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
                        cx.emit(StudioEvent::ShowThread {
                            conversation_id: conversation.id,
                            focus_artifact: None,
                        });
                        cx.emit(StudioEvent::SidebarChanged);
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn current_studio_prompts(&self) -> Vec<PromptHistoryItem> {
        self.conversation
            .as_ref()
            .map(|view| studio_prompt_history(&view.turns))
            .unwrap_or_default()
    }

    fn snap_prompt_history(&mut self, cx: &mut Context<Self>) {
        let prompts = self.current_studio_prompts();
        if self.prompt_history.snap_if_vanished(&prompts) {
            let scratch = self.prompt_history.scratch().to_string();
            self.prompt.update(cx, |input, cx| {
                input.replace_from_history(scratch, false, cx);
            });
        }
    }

    fn on_history_navigate(&mut self, dir: isize, cx: &mut Context<Self>) {
        let prompts = self.current_studio_prompts();
        let current_text = self.prompt.read(cx).text().to_string();
        if self.prompt_history.snap_if_vanished(&prompts) {
            let scratch = self.prompt_history.scratch().to_string();
            self.prompt.update(cx, |input, cx| {
                input.replace_from_history(scratch, false, cx);
            });
            return;
        }
        let fill = if dir < 0 {
            self.prompt_history.up(&prompts, &current_text)
        } else {
            self.prompt_history.down(&prompts, &current_text)
        };
        let Some(fill) = fill else {
            return;
        };
        self.prompt.update(cx, |input, cx| {
            input.replace_from_history(fill.text, fill.caret_at_start, cx);
        });
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
        if prompt.is_empty() || self.busy {
            return;
        }
        self.sync_prompt_into_composer(cx);
        // Engine requires request.prompt == composer.prompt; do not send a trimmed
        // request next to an untrimmed snapshot.
        self.composer.prompt = prompt.clone();
        if !self.composer_view.send.enabled {
            if let Some(conflict) = self
                .composer_view
                .conflicts
                .iter()
                .find(|conflict| conflict.blocks_send())
            {
                self.popup_conflict = Some(conflict.id.clone());
                self.conflict_more_open = false;
                cx.notify();
            }
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
        let composer = self.composer.clone();
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
        self.feed_scroll_to_end();
        let batch_quote = super::cost::selected_batch_quote(
            &self.models,
            &self.selected_models,
            &self.draft_runs,
            &self.live_quotes,
        );
        if let Some(quote) = batch_quote.as_ref() {
            self.reserve_account_spend(quote);
            cx.notify();
        }
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::CREATE_STUDIO_TURN,
                    serde_json::json!({
                        "conversationId": conversation_id, "prompt": prompt, "runs": runs,
                        "sourceTurnId": source, "composer": composer,
                    }),
                )
                .await;
            let models = if let Some(provider_id) = provider_id {
                engine
                    .client()
                    .call(
                        methods::LIST_STUDIO_MODELS,
                        serde_json::json!({ "providerId": provider_id }),
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
                        page.prompt_history.reset();
                        page.source_turn = None;
                        page.persist_composer_defaults(cx);
                    }
                    Err(error) => {
                        page.scroll_after_turn_count = None;
                        if let Some(quote) = batch_quote.as_ref() {
                            page.release_account_spend(quote);
                        }
                        page.apply_studio_rpc_error(error);
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn apply_models(&mut self, models: Vec<zeron_studio::MediaModel>) {
        let (upscale_models, rest): (Vec<_>, Vec<_>) = models
            .into_iter()
            .partition(|model| model.operation == zeron_studio::MediaOperation::Upscale);
        let (edit_models, models): (Vec<_>, Vec<_>) = rest
            .into_iter()
            .partition(|model| model.operation == zeron_studio::MediaOperation::ImageEdit);
        self.upscale_models = upscale_models;
        self.edit_models = edit_models;
        self.models = models;
        apply_remembered_drafts(&mut self.draft_runs, &self.models, &self.remembered.drafts);
        self.reevaluate_composer(None);
    }

    pub(super) fn apply_remembered_or_default_models(&mut self, cx: &mut Context<Self>) {
        let mode = self.remembered.last_mode;
        if mode == ComposerMode::Video {
            self.composer.duration = self.remembered.video_duration.clone();
        }
        let restore = self.restore_refs_for(mode);
        self.apply_composer_event(ComposerEvent::SetMode { mode, restore }, None, cx);
    }

    pub(super) fn seed_composer_from_conversation(&mut self, cx: &mut Context<Self>) {
        let Some((conversation_id, last_turn)) = self.conversation.as_ref().and_then(|view| {
            let conversation_id = view.conversation.id;
            if self.selected_conversation != Some(conversation_id)
                || self.composer_seeded_for == Some(conversation_id)
            {
                return None;
            }
            Some((conversation_id, view.turns.last().cloned()))
        }) else {
            return;
        };
        if let Some(turn) = last_turn.as_ref()
            && let Some(mut snapshot) =
                snapshot_from_committed_turn(turn, &self.models, &self.known_studio_artifacts())
        {
            // Opening a thread restores chips/tray, not the last prompt.
            // Leave source_turn unset so "Using previous settings" is only
            // shown after an explicit Use prompt.
            snapshot.prompt = self.prompt.read(cx).text().to_owned();
            snapshot.source_turn_id = None;
            self.apply_composer_event(ComposerEvent::RestoreDraft { snapshot }, None, cx);
        } else {
            self.apply_remembered_or_default_models(cx);
        }
        self.composer_seeded_for = Some(conversation_id);
        self.persist_composer_defaults(cx);
    }

    pub(super) fn persist_composer_defaults(&mut self, cx: &mut Context<Self>) {
        if let Some(dir) = self.state.read(cx).data_dir.clone() {
            let (image_ids, video_ids) = self.remembered_mode_lists();
            let video_duration = match self.composer.mode {
                ComposerMode::Video => self.composer.duration.clone(),
                ComposerMode::Image => self.remembered.video_duration.clone(),
            };
            self.remembered = StudioDefaults::capture(
                &image_ids,
                &video_ids,
                &self.draft_runs,
                &self.remembered.favorites,
                &self.remembered.upscale,
                video_duration,
                self.composer.mode,
                self.remembered.last_edit_model_id.clone(),
            );
            if let Err(err) = self.remembered.save(&dir) {
                tracing::warn!(error = %err, "studio-defaults save failed");
            }
        }
        self.sync_prompt_placeholder(cx);
        self.refresh_draft_quotes(cx);
    }

    pub(super) fn refresh_draft_quotes(&mut self, cx: &mut Context<Self>) {
        self.live_quotes
            .retain(|id, _| self.selected_models.contains(id));
        if !super::cost::needs_live_quote(&self.models, &self.selected_models, &self.draft_runs) {
            self.quote_task = None;
            return;
        }
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.sync_prompt_into_composer(cx);
        let prompt = self.composer.prompt.clone();
        let composer = self.composer.clone();
        if composer.selected.is_empty() {
            return;
        }
        self.quote_generation = self.quote_generation.saturating_add(1);
        let generation = self.quote_generation;
        self.quote_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(250))
                .await;
            let result = engine
                .client()
                .call(
                    methods::QUOTE_STUDIO_BATCH,
                    serde_json::json!({ "prompt": prompt, "composer": composer }),
                )
                .await;
            this.update(cx, |page, cx| {
                if page.quote_generation != generation {
                    return;
                }
                if let Ok(value) = result
                    && let Ok(response) = serde_json::from_value::<QuoteStudioBatchResponse>(value)
                {
                    page.live_quotes = response
                        .runs
                        .into_iter()
                        .filter_map(|run| run.quote.map(|quote| (run.model_id, quote)))
                        .collect();
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub(super) fn toggle_prompt_expanded(
        &mut self,
        turn_id: zeron_studio::StudioTurnId,
        cx: &mut Context<Self>,
    ) {
        if !self.expanded_prompts.remove(&turn_id) {
            self.expanded_prompts.insert(turn_id);
        }
        if let Some(ix) = self.feed_index_of_turn(turn_id) {
            self.feed_list.remeasure_items(ix..ix + 1);
        }
        cx.notify();
    }

    pub(super) fn toggle_inspector_prompt_expanded(
        &mut self,
        turn_id: zeron_studio::StudioTurnId,
        cx: &mut Context<Self>,
    ) {
        if !self.expanded_inspector_prompts.remove(&turn_id) {
            self.expanded_inspector_prompts.insert(turn_id);
        }
        cx.notify();
    }

    fn known_studio_artifacts(&self) -> Vec<zeron_proto::StudioArtifactView> {
        self.conversation
            .as_ref()
            .map(|view| {
                view.turns
                    .iter()
                    .flat_map(|turn| {
                        turn.runs
                            .iter()
                            .flat_map(|run| run.artifacts.iter().cloned())
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(super) fn use_prompt(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
        self.prompt_history.reset();
        self.prompt
            .update(cx, |input, cx| input.set_text(turn.prompt.clone(), cx));
        if let Some(snapshot) =
            snapshot_from_committed_turn(turn, &self.models, &self.known_studio_artifacts())
        {
            self.apply_composer_event(ComposerEvent::RestoreDraft { snapshot }, None, cx);
        }
        if let Some(conversation_id) = self.selected_conversation {
            self.composer_seeded_for = Some(conversation_id);
        }
        self.source_turn = Some(turn.id);
        self.persist_composer_defaults(cx);
        cx.notify();
    }

    /// Latest turn that can accept another generate-more batch. Empty-run
    /// turns (no model specs to copy) stay hidden from the composer pill.
    pub(super) fn latest_extendable_turn(&self) -> Option<&StudioTurnView> {
        self.conversation
            .as_ref()
            .and_then(|view| view.turns.last())
            .filter(|turn| !turn.runs.is_empty())
    }

    pub(super) fn generate_more_latest(&mut self, cx: &mut Context<Self>) {
        let Some(turn) = self.latest_extendable_turn().cloned() else {
            return;
        };
        self.generate_more(&turn, cx);
    }

    pub(super) fn generate_more(&mut self, turn: &StudioTurnView, cx: &mut Context<Self>) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        if self.busy {
            return;
        }
        let turn_id = turn.id;
        let is_last = self
            .conversation
            .as_ref()
            .and_then(|view| view.turns.last())
            .is_some_and(|last| last.id == turn_id);
        if is_last {
            self.scroll_after_extend = Some(turn_id);
            self.feed_scroll_to_end();
        }
        let quote = super::cost::turn_quote(turn);
        if let Some(quote) = quote.as_ref() {
            self.reserve_account_spend(quote);
            cx.notify();
        }
        self.busy = true;
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::EXTEND_STUDIO_TURN,
                    serde_json::json!({ "turnId": turn_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                page.busy = false;
                if let Err(error) = result {
                    page.scroll_after_extend = None;
                    if let Some(quote) = quote.as_ref() {
                        page.release_account_spend(quote);
                    }
                    page.apply_studio_rpc_error(error);
                }
                cx.notify();
            })
            .ok();
        }));
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
                        cx.emit(StudioEvent::ShowThread {
                            conversation_id: conversation.id,
                            focus_artifact: None,
                        });
                    }
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn rename_conversation(
        &mut self,
        conversation_id: StudioConversationId,
        title: String,
        cx: &mut Context<Self>,
    ) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        self.action_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::RENAME_STUDIO_CONVERSATION,
                    serde_json::json!({ "conversationId": conversation_id, "title": title }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result.and_then(|value| {
                    serde_json::from_value::<StudioConversationSummary>(value)
                        .map_err(|error| zeron_rpc::RpcError::Failed(error.to_string()))
                }) {
                    Ok(summary) => page.apply_conversation_summary(summary, cx),
                    Err(error) => page.error = Some(error.to_string().into()),
                }
                cx.notify();
            })
            .ok();
        }));
    }

    pub fn delete_conversation(
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
                    methods::DELETE_STUDIO_CONVERSATION,
                    serde_json::json!({ "conversationId": conversation_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                match result {
                    Ok(_) => {
                        page.conversations.retain(|item| item.id != conversation_id);
                        if page.selected_conversation == Some(conversation_id) {
                            page.close_artifact(cx);
                            page.selected_conversation = None;
                            page.conversation = None;
                            page.prompt_history.reset();
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
                            page.prompt_history.reset();
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
        let quote = self.conversation.as_ref().and_then(|view| {
            view.turns
                .iter()
                .flat_map(|turn| turn.runs.iter())
                .find(|run| run.id == run_id)
                .and_then(|run| run.quote.clone())
        });
        if let Some(quote) = quote.as_ref() {
            self.reserve_account_spend(quote);
            cx.notify();
        }
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
                    if let Some(quote) = quote.as_ref() {
                        page.release_account_spend(quote);
                    }
                    page.error = Some(error.to_string().into());
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn configured_provider_id(&self) -> Option<zeron_studio::ProviderId> {
        self.providers
            .iter()
            .find(|provider| provider.configured)
            .map(|provider| provider.provider_id.clone())
    }

    fn reserve_account_spend(&mut self, quote: &zeron_studio::Quote) {
        self.reserved_spend = match self.reserved_spend.as_ref() {
            Some(existing) => existing
                .saturating_add(quote)
                .or_else(|| Some(quote.clone())),
            None => Some(quote.clone()),
        };
    }

    fn release_account_spend(&mut self, quote: &zeron_studio::Quote) {
        self.reserved_spend = self
            .reserved_spend
            .as_ref()
            .and_then(|existing| existing.saturating_sub(quote))
            .filter(|remaining| remaining.amount > 0.0);
    }

    fn refresh_account_balance(&mut self, cx: &mut Context<Self>, clear_reserved: bool) {
        let Some(engine) = self.engine(cx) else {
            return;
        };
        let Some(provider_id) = self.configured_provider_id() else {
            self.account_balance = None;
            self.reserved_spend = None;
            return;
        };
        self.balance_generation = self.balance_generation.saturating_add(1);
        let generation = self.balance_generation;
        self.balance_task = Some(cx.spawn(async move |this, cx| {
            let result = engine
                .client()
                .call(
                    methods::GET_STUDIO_PROVIDER_BALANCE,
                    serde_json::json!({ "providerId": provider_id }),
                )
                .await;
            this.update(cx, |page, cx| {
                if page.balance_generation != generation {
                    return;
                }
                if let Ok(value) = result
                    && let Ok(response) =
                        serde_json::from_value::<StudioProviderBalanceResponse>(value)
                {
                    page.account_balance = response.balance;
                    if clear_reserved {
                        page.reserved_spend = None;
                    }
                }
                cx.notify();
            })
            .ok();
        }));
    }
}

fn newly_settled_runs(previous: &StudioConversationView, next: &StudioConversationView) -> bool {
    let before = terminal_run_ids(previous);
    terminal_run_ids(next).difference(&before).next().is_some()
}

fn first_newly_failed_message(
    previous: &StudioConversationView,
    next: &StudioConversationView,
) -> Option<SharedString> {
    let before: HashMap<zeron_studio::StudioRunId, StudioRunState> = previous
        .turns
        .iter()
        .flat_map(|turn| turn.runs.iter())
        .map(|run| (run.id, run.state))
        .collect();
    next.turns
        .iter()
        .flat_map(|turn| turn.runs.iter())
        .find(|run| {
            run.state == StudioRunState::Failed
                && before.get(&run.id).copied() != Some(StudioRunState::Failed)
        })
        .map(format_run_failure)
}

fn format_run_failure(run: &StudioRunView) -> SharedString {
    let detail = run_error_message(run.error.as_deref());
    match run.model.operation {
        MediaOperation::Upscale => match detail {
            Some(message) => format!("Upscale failed: {message}").into(),
            None => "Upscale failed".into(),
        },
        MediaOperation::ImageEdit => match detail {
            Some(message) => format!("Edit failed: {message}").into(),
            None => "Edit failed".into(),
        },
        _ => match detail {
            Some(message) => message.to_string().into(),
            None => "Generation failed".into(),
        },
    }
}

fn terminal_run_ids(view: &StudioConversationView) -> HashSet<zeron_studio::StudioRunId> {
    view.turns
        .iter()
        .flat_map(|turn| turn.runs.iter())
        .filter(|run| {
            matches!(
                run.state,
                StudioRunState::Succeeded | StudioRunState::Failed | StudioRunState::Cancelled
            )
        })
        .map(|run| run.id)
        .collect()
}

impl EventEmitter<StudioEvent> for StudioPage {}
impl Focusable for StudioPage {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Render for StudioPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.sync_lineage();
        if self.video_needs_frame() && !self.video_frame_scheduled {
            self.video_frame_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |window, cx| {
                entity
                    .update(cx, |page: &mut StudioPage, cx| {
                        page.video_frame_scheduled = false;
                        page.step_video_frame(window, cx);
                    })
                    .ok();
            });
        }
        if self.lightbox_motion_pending() && crate::motion::reduced_motion(cx) {
            self.finish_lightbox_snap_immediate(cx);
        } else if self.lightbox_motion_pending() && !self.lightbox_swipe_scheduled {
            self.lightbox_swipe_scheduled = true;
            let entity = cx.weak_entity();
            window.on_next_frame(move |_, cx| {
                entity
                    .update(cx, |page: &mut StudioPage, cx| {
                        page.lightbox_swipe_scheduled = false;
                        page.step_lightbox_motion(cx);
                    })
                    .ok();
            });
        }
        let theme = Theme::of(cx).clone();
        self.images.flush(Some(window), cx);
        let body = if let Some(page) = self.render_artifact_page(&theme, window, cx) {
            page
        } else if self.providers.iter().all(|provider| !provider.configured) {
            // Parent is not a flex container, so `flex_1` cannot fill it.
            // Same canvas as the chat "Add a project" empty state.
            div()
                .size_full()
                .pt(px(Theme::TITLEBAR_HEIGHT))
                .flex()
                .flex_col()
                .items_center()
                .justify_center()
                .child(crate::motion::fade_in(
                    "studio-no-provider-canvas",
                    div()
                        .flex()
                        .flex_col()
                        .items_center()
                        .child(
                            crate::icons::icon(crate::icons::ZERON_LOGO)
                                .w(px(41.9))
                                .h(px(48.0))
                                .text_color(theme.text.opacity(0.09)),
                        )
                        .child(
                            div()
                                .mt(px(24.0))
                                .text_size(px(16.0))
                                .font_weight(gpui::FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child("Connect a provider to begin"),
                        )
                        .child(
                            div()
                                .mt(px(6.0))
                                .text_size(px(13.0))
                                .text_color(theme.text_muted.opacity(0.7))
                                .child("Connect an image provider to start generating."),
                        )
                        .child(
                            popover::btn_primary(&theme, "Add provider")
                                .id("studio-add-provider")
                                .mt(px(20.0))
                                .on_click(cx.listener(|_, _, _, cx| {
                                    cx.emit(StudioEvent::OpenProviders);
                                })),
                        ),
                ))
                .into_any_element()
        } else if self.selected_conversation.is_none() {
            self.render_gallery(window, &theme, cx)
        } else {
            let file_drag_active = self.file_drag_active && cx.has_active_drag();
            let veil = file_drag_active.then(|| {
                div()
                    .absolute()
                    .inset_0()
                    .bg(theme.scrim().opacity(0.4 / 0.6))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(px(13.0))
                    .text_color(theme.text)
                    .child(SharedString::from(super::tray::studio_drop_veil_copy(
                        &self.composer_view.attachments.accept,
                    )))
            });
            div()
                .id("studio-dropzone")
                .relative()
                .flex_1()
                .min_w_0()
                .h_full()
                .overflow_hidden()
                .on_drag_move::<ExternalPaths>(cx.listener(
                    |this, e: &DragMoveEvent<ExternalPaths>, _, cx| {
                        let inside = e.bounds.contains(&e.event.position);
                        if this.file_drag_active != inside {
                            this.file_drag_active = inside;
                            cx.notify();
                        }
                    },
                ))
                .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                    this.file_drag_active = false;
                    this.add_dropped_paths(paths.paths().to_vec(), cx);
                    cx.notify();
                }))
                .child(self.render_conversation_feed(window, &theme, cx))
                .child(self.render_composer(window, &theme, cx))
                .children(veil)
                .into_any_element()
        };
        if std::mem::take(&mut self.tray_preview_focus_pending) {
            window.focus(&self.tray_preview_focus, cx);
        }
        let tray_lightbox = self.tray_preview.clone().map(|preview| {
            let weak = cx.weak_entity();
            crate::attachments::lightbox(
                window.viewport_size(),
                &preview,
                &self.tray_preview_focus,
                move |window, cx| {
                    if let Ok(focus) = weak.update(cx, |page, cx| {
                        page.tray_preview = None;
                        cx.notify();
                        page.focus.clone()
                    }) {
                        window.focus(&focus, cx);
                    }
                },
            )
        });
        div()
            .relative()
            .size_full()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|page, event: &gpui::KeyDownEvent, window, cx| {
                if page.dismiss_image_menu(event, cx) {
                    cx.stop_propagation();
                } else if page.dismiss_artifact_actions_menu(event, cx) {
                    cx.stop_propagation();
                } else if page.dismiss_upscale_settings_menu(event, cx) {
                    cx.stop_propagation();
                } else if page.on_edit_model_picker_key(event, window, cx) {
                    cx.stop_propagation();
                }
            }))
            .child(body)
            .when_some(self.error.clone(), |el, error| {
                el.child(
                    div()
                        .id("studio-error")
                        .absolute()
                        .top(px(12.0))
                        .right(px(12.0))
                        .w(px(420.0))
                        .max_w(gpui::relative(0.92))
                        .cursor_pointer()
                        .on_click(cx.listener(|page, _, _, cx| {
                            page.error = None;
                            cx.notify();
                        }))
                        .child(render_run_error_chip(&theme, error.as_ref())),
                )
            })
            .when_some(self.render_image_menu(&theme, cx), |el, menu| {
                el.child(menu)
            })
            .children(tray_lightbox)
    }
}

/// Visible turn prompts, oldest first. Blank bodies are skipped so a recall
/// never fills the composer with nothing.
fn studio_prompt_history(turns: &[StudioTurnView]) -> Vec<PromptHistoryItem> {
    turns
        .iter()
        .filter(|turn| !turn.prompt.trim().is_empty())
        .map(|turn| PromptHistoryItem {
            message_id: turn.id.0.to_string(),
            text: turn.prompt.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use zeron_studio::{StudioBatchId, StudioTurnId};

    fn turn(id: StudioTurnId, prompt: &str) -> StudioTurnView {
        StudioTurnView {
            id,
            position: 0,
            prompt: prompt.into(),
            source_turn_id: None,
            batch_id: StudioBatchId::new(),
            runs: Vec::new(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn studio_history_lists_visible_turn_prompts_oldest_first() {
        let older = StudioTurnId::new();
        let newer = StudioTurnId::new();
        let items = studio_prompt_history(&[
            turn(older, "first look"),
            turn(StudioTurnId::new(), "   "),
            turn(newer, "second look"),
        ]);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].message_id, older.0.to_string());
        assert_eq!(items[0].text, "first look");
        assert_eq!(items[1].message_id, newer.0.to_string());
        assert_eq!(items[1].text, "second look");
    }

    #[test]
    fn studio_history_up_stashes_the_draft() {
        let items = studio_prompt_history(&[
            turn(StudioTurnId::new(), "older"),
            turn(StudioTurnId::new(), "newer"),
        ]);
        let mut hist = PromptHistory::default();
        let fill = hist.up(&items, "unsent draft").unwrap();
        assert_eq!(fill.text, "newer");
        assert!(fill.caret_at_start);
        let fill = hist.down(&items, "").unwrap();
        assert_eq!(fill.text, "unsent draft");
        assert!(!fill.caret_at_start);
    }

    fn conversation_with_run(
        run_id: zeron_studio::StudioRunId,
        state: StudioRunState,
        operation: MediaOperation,
        error: Option<&str>,
    ) -> StudioConversationView {
        StudioConversationView {
            conversation: StudioConversationSummary {
                id: StudioConversationId::new(),
                title: "one".into(),
                turn_count: 1,
                created_at: Utc::now(),
                updated_at: Utc::now(),
                archived: false,
                forked_from_turn_id: None,
                creating: false,
                done: false,
            },
            turns: vec![StudioTurnView {
                id: StudioTurnId::new(),
                position: 0,
                prompt: "a clip".into(),
                source_turn_id: None,
                batch_id: StudioBatchId::new(),
                created_at: Utc::now(),
                runs: vec![StudioRunView {
                    id: run_id,
                    position: 0,
                    provider_id: "venice".into(),
                    model: zeron_studio::MediaModel {
                        provider_id: "venice".into(),
                        id: "seedance".into(),
                        display_name: "Seedance".into(),
                        description: None,
                        operation,
                        output_kind: zeron_studio::MediaKind::Video,
                        output_mime_types: vec!["video/mp4".into()],
                        input_constraints: Vec::new(),
                        prompt_maximum_chars: None,
                        negative_prompt_maximum_chars: None,
                        maximum_output_count: 1,
                        controls: Vec::new(),
                        pricing: None,
                        features: Vec::new(),
                        video: zeron_studio::VideoModelMeta::default(),
                        manifest_version: "test".into(),
                        fetched_at: Utc::now(),
                    },
                    controls: Default::default(),
                    output_count: 1,
                    display_aspect_ratio: (9, 16),
                    state,
                    progress: None,
                    error: error.map(str::to_string),
                    quote: None,
                    prompt: None,
                    inputs: Vec::new(),
                    artifacts: Vec::new(),
                }],
            }],
        }
    }

    #[test]
    fn newly_failed_run_surfaces_the_provider_message() {
        let run_id = zeron_studio::StudioRunId::new();
        let previous = conversation_with_run(
            run_id,
            StudioRunState::Running,
            MediaOperation::ReferenceToVideo,
            None,
        );
        let next = conversation_with_run(
            run_id,
            StudioRunState::Failed,
            MediaOperation::ReferenceToVideo,
            Some("Your prompt violates the content policy of Venice.ai or the model provider"),
        );
        assert_eq!(
            first_newly_failed_message(&previous, &next).as_deref(),
            Some("Your prompt violates the content policy of Venice.ai or the model provider")
        );
        assert_eq!(first_newly_failed_message(&next, &next), None);
    }

    #[test]
    fn newly_failed_upscale_keeps_its_prefix() {
        let run_id = zeron_studio::StudioRunId::new();
        let previous = conversation_with_run(
            run_id,
            StudioRunState::Running,
            MediaOperation::Upscale,
            None,
        );
        let next = conversation_with_run(
            run_id,
            StudioRunState::Failed,
            MediaOperation::Upscale,
            Some("pixel limit"),
        );
        assert_eq!(
            first_newly_failed_message(&previous, &next).as_deref(),
            Some("Upscale failed: pixel limit")
        );
    }

    #[test]
    fn failed_run_without_provider_copy_uses_a_generic_label() {
        let run = conversation_with_run(
            zeron_studio::StudioRunId::new(),
            StudioRunState::Failed,
            MediaOperation::TextToImage,
            None,
        );
        assert_eq!(
            format_run_failure(&run.turns[0].runs[0]).as_ref(),
            "Generation failed"
        );
    }
}
