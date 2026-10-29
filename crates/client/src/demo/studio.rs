//! Demo Studio: a desktop engine's Studio (generated images and video), in
//! memory on the demo Mac.
//!
//! Every reply is a real `zeron_proto` / `zeron_studio` value serialized the
//! way the engine serializes it, so the app decodes exactly the engine's wire
//! format. The model catalog is the real Venice normalizer run over a canned
//! `GET /models` response, the composer is the real evaluator, and threads,
//! gallery pages and media chunks follow the engine store's semantics
//! (ordering, paging cursors, `creating`/`done`, 192 kB chunks, error text).
//! Watches push a fresh snapshot on every change, like the store's change
//! channel. Runs play out on the host's clock: image runs stay queued until
//! they land 2–4 s later (Venice images are synchronous); video runs report
//! progress, then download. A prompt with the word "fail" fails the way an
//! empty Venice account does.
//!
//! Faked: artifact bytes are painted on demand ([`png::art`]), so
//! `contentHash` digests the painting's recipe rather than the bytes; video
//! originals can't be produced (no encoder here), so reading one fails with a
//! clear error, though their posters preview fine; attachments can't be
//! imported.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;
use zeron_proto::studio::{
    AppendStudioDerivedRunRequest, ArchiveStudioConversationRequest, CheckStudioRunRequest,
    CreateStudioConversationRequest, CreateStudioTurnRequest, DeleteStudioArtifactRequest,
    DeleteStudioConversationRequest, EvaluateStudioComposerRequest, ExtendStudioTurnRequest,
    LEGACY_UNTITLED_STUDIO_TITLE, ListStudioArtifactsRequest, ListStudioArtifactsResponse,
    ListStudioConversationsRequest, ListStudioConversationsResponse, ListStudioModelsRequest,
    ListStudioModelsResponse, ListStudioProvidersResponse, MarkStudioConversationSeenRequest,
    ProviderValidationState, QuoteStudioBatchRequest, QuoteStudioBatchResponse, QuoteStudioRunView,
    ReadStudioArtifactChunkRequest, RenameStudioConversationRequest, RetryStudioRunRequest,
    SetStudioProviderCredentialRequest, SetStudioProviderPreferencesRequest, StudioArtifactChunk,
    StudioArtifactView, StudioConversationSummary, StudioConversationView, StudioGalleryCursor,
    StudioGalleryItem, StudioModelRunSpec, StudioProviderBalanceResponse, StudioProviderConnection,
    StudioProviderRequest, StudioRunState, StudioRunView, StudioTurnView, StudioValidationError,
    UNTITLED_STUDIO_TITLE, WatchStudioConversationRequest,
};
use zeron_rpc::{RpcError, methods as m};
use zeron_studio::{
    AccountBalance, ComposerConflict, ComposerMode, ComposerSnapshot, ControlId, ControlValue,
    GenerationInput, GenerationInputSource, GenerationRequest, MediaKind, MediaModel,
    MediaOperation, ProviderId, Quote, ROLE_SOURCE, StudioArtifactId, StudioBatchId,
    StudioConversationId, StudioRunId, StudioTurnId,
};

use super::png;
use crate::lock;

/// The engine's `ARTIFACT_READ_CHUNK_BYTES`.
const CHUNK_BYTES: usize = 192_000;
/// The engine's gallery preview bounds (`studio_preview`).
const THUMB_SHORT_EDGE: u32 = 512;
const THUMB_LONG_EDGE: u32 = 1536;
/// Originals are painted at Venice's 1K long edge, whatever the resolution
/// control says: stored PNGs are big, and 4K ones would be absurd.
const ORIGINAL_LONG_EDGE: u32 = 1024;
const MAX_CREATE_TURN_RUNS: usize = 16;
const MAX_TURN_RUNS: usize = 64;

const VENICE: &str = zeron_studio::venice::VENICE_PROVIDER_ID;
const NOT_CONFIGURED: &str = "studio provider credential is not configured";
/// What Venice says when the account runs dry (HTTP 402).
const INSUFFICIENT: &str = "Insufficient USD or Diem balance to complete request";
const CONVERSATION_NOT_FOUND: &str = "studio conversation was not found";
const TURN_NOT_FOUND: &str = "studio turn was not found";
const RUN_NOT_FOUND: &str = "studio run was not found";
const ARTIFACT_NOT_FOUND: &str = "studio artifact was not found";

/// Studio RPCs (calls and watches) — all of them carry "Studio" in the name.
pub(crate) fn is_studio(method: &str) -> bool {
    method.contains("Studio")
}

/// A canned Venice `GET /api/v1/models` page: four image models and Seedance
/// text-to-video (the bundled overlay makes it picker-visible).
const VENICE_MODELS: &str = r#"{
  "object": "list",
  "data": [
    {
      "id": "gpt-image-2", "type": "image", "object": "model", "owned_by": "venice.ai",
      "model_spec": {
        "name": "GPT Image 2",
        "privacy": "anonymized",
        "model_sets": ["venice_recommendations", "featured"],
        "pricing": {
          "resolutions": {
            "1K": { "usd": 0.27, "diem": 0.27 },
            "2K": { "usd": 0.51, "diem": 0.51 },
            "4K": { "usd": 0.84, "diem": 0.84 }
          },
          "quality": {
            "1K": {
              "high": { "usd": 0.26, "diem": 0.26 },
              "low": { "usd": 0.02, "diem": 0.02 },
              "medium": { "usd": 0.07, "diem": 0.07 }
            }
          }
        },
        "constraints": {
          "promptCharacterLimit": 10000,
          "defaultResolution": "1K", "resolutions": ["1K", "2K", "4K"],
          "defaultQuality": "high", "qualities": ["low", "medium", "high"],
          "aspectRatios": ["1:1", "3:2", "16:9", "21:9", "9:16", "2:3", "3:4", "4:5"],
          "defaultAspectRatio": "1:1",
          "steps": { "default": 20, "max": 50 }
        }
      }
    },
    {
      "id": "seedream-v4", "type": "image", "object": "model", "owned_by": "venice.ai",
      "model_spec": {
        "name": "Seedream 4",
        "description": "ByteDance's photoreal generator with strong typography.",
        "privacy": "anonymized",
        "pricing": {
          "resolutions": {
            "1K": { "usd": 0.05, "diem": 0.05 },
            "2K": { "usd": 0.05, "diem": 0.05 },
            "4K": { "usd": 0.08, "diem": 0.08 }
          }
        },
        "constraints": {
          "promptCharacterLimit": 4000,
          "defaultResolution": "2K", "resolutions": ["1K", "2K", "4K"],
          "aspectRatios": ["1:1", "4:3", "3:4", "16:9", "9:16", "3:2", "2:3", "4:5", "21:9"],
          "defaultAspectRatio": "1:1",
          "steps": { "default": 20, "max": 50 }
        }
      }
    },
    {
      "id": "qwen-image", "type": "image", "object": "model", "owned_by": "venice.ai",
      "model_spec": {
        "name": "Qwen Image",
        "privacy": "private",
        "pricing": { "generation": { "usd": 0.01, "diem": 0.01 } },
        "constraints": {
          "promptCharacterLimit": 1500,
          "aspectRatios": ["1:1", "16:9", "9:16", "4:3", "3:4", "3:2", "2:3", "4:5"],
          "defaultAspectRatio": "1:1",
          "steps": { "default": 8, "max": 50 }
        }
      }
    },
    {
      "id": "venice-sd35", "type": "image", "object": "model", "owned_by": "venice.ai",
      "model_spec": {
        "name": "Venice SD35",
        "privacy": "private",
        "uncensored": true,
        "pricing": { "generation": { "usd": 0.01, "diem": 0.01 } },
        "constraints": {
          "promptCharacterLimit": 1500,
          "aspectRatios": ["1:1", "3:2", "2:3", "16:9", "9:16", "4:5", "3:4"],
          "defaultAspectRatio": "1:1",
          "formats": ["webp", "png"], "defaultFormat": "webp",
          "steps": { "default": 25, "max": 30 }
        }
      }
    },
    {
      "id": "seedance-1-5-pro-text-to-video-basic", "type": "video", "object": "model",
      "owned_by": "venice.ai",
      "model_spec": {
        "name": "Seedance 1.5 Pro",
        "privacy": "anonymized",
        "uncensored": true,
        "constraints": {
          "model_type": "text-to-video",
          "aspect_ratios": ["21:9", "16:9", "4:3", "1:1", "3:4", "9:16"],
          "resolutions": ["1080p", "720p", "480p"],
          "durations": ["4s", "5s", "6s", "7s", "8s", "9s", "10s", "11s", "12s"],
          "audio": true, "audio_configurable": true,
          "prompt_character_limit": 3500
        }
      }
    }
  ]
}"#;

const GPT: &str = "gpt-image-2";
const SEEDREAM: &str = "seedream-v4";
const QWEN: &str = "qwen-image";
const SD35: &str = "venice-sd35";
const SEEDANCE: &str = "seedance-1-5-pro-text-to-video-basic";

fn catalog(fetched_at: DateTime<Utc>) -> Vec<MediaModel> {
    zeron_studio::venice::normalize_model_catalog(VENICE_MODELS.as_bytes(), fetched_at)
        .unwrap_or_default()
}

// ── seed ───────────────────────────────────────────────────────────────────

enum Outcome {
    Done,
    Failed(&'static str),
    /// Still generating at this progress; plays out once Studio is opened.
    Running(f32),
}

/// Model, aspect ratio, output count, how it ended.
struct SeedRun(&'static str, (u32, u32), u32, Outcome);
struct SeedTurn(&'static str, &'static [SeedRun]);

struct SeedThread {
    title: &'static str,
    archived: bool,
    /// A run finished after the user last looked (the "Done" badge).
    unseen: bool,
    updated_ago_min: i64,
    /// Minutes between turns.
    pace_min: i64,
    style: u32,
    palettes: [u32; 2],
    turns: &'static [SeedTurn],
}

const fn run(model: &'static str, aspect: (u32, u32), outputs: u32) -> SeedRun {
    SeedRun(model, aspect, outputs, Outcome::Done)
}

const SQUARE: (u32, u32) = (1, 1);
const WIDE: (u32, u32) = (16, 9);

const THREADS: &[SeedThread] = &[
    SeedThread {
        title: "Neon koi pond at night",
        archived: false,
        unseen: false,
        updated_ago_min: 3,
        pace_min: 6,
        style: png::STYLE_BLOBS,
        palettes: [2, 5],
        turns: &[
            SeedTurn(
                "Neon koi pond at night, paper lanterns reflecting on black water, cinematic",
                &[run(GPT, WIDE, 4), run(SEEDREAM, WIDE, 2)],
            ),
            SeedTurn(
                "Slow push-in over the koi pond, lanterns swaying, light rain",
                &[SeedRun(SEEDANCE, WIDE, 1, Outcome::Running(0.35))],
            ),
        ],
    },
    SeedThread {
        title: "Isometric cozy reading nook",
        archived: false,
        unseen: true,
        updated_ago_min: 25,
        pace_min: 5,
        style: png::STYLE_TILES,
        palettes: [3, 4],
        turns: &[
            SeedTurn(
                "Isometric cozy reading nook with plants and a warm lamp, soft clay render",
                &[run(GPT, SQUARE, 4), run(QWEN, SQUARE, 2)],
            ),
            SeedTurn(
                "Same nook at night, rain on the window",
                &[run(GPT, SQUARE, 4)],
            ),
            SeedTurn(
                "Top-down floor plan version, pastel colors",
                &[run(QWEN, SQUARE, 3)],
            ),
            SeedTurn(
                "Make it a tiny attic room with a round window",
                &[run(SEEDREAM, SQUARE, 4)],
            ),
        ],
    },
    SeedThread {
        title: "Brutalist architecture studies",
        archived: false,
        unseen: false,
        updated_ago_min: 5 * 60,
        pace_min: 9,
        style: png::STYLE_TILES,
        palettes: [7, 5],
        turns: &[
            SeedTurn(
                "Brutalist concrete library in fog, wide angle",
                &[run(SEEDREAM, WIDE, 4)],
            ),
            SeedTurn(
                "Interior atrium with skylights and long shadows",
                &[run(GPT, (3, 4), 4)],
            ),
            SeedTurn(
                "Night exterior under sodium street lights",
                &[run(QWEN, WIDE, 4), run(SD35, WIDE, 2)],
            ),
            SeedTurn("Axonometric line drawing", &[run(GPT, SQUARE, 2)]),
        ],
    },
    SeedThread {
        title: "Mountain lake at golden hour",
        archived: false,
        unseen: false,
        updated_ago_min: 26 * 60,
        pace_min: 11,
        style: png::STYLE_LANDSCAPE,
        palettes: [0, 1],
        turns: &[
            SeedTurn(
                "Alpine lake at golden hour, mirror reflections, 35mm film",
                &[run(SEEDREAM, (3, 2), 4), run(GPT, (3, 2), 2)],
            ),
            SeedTurn(
                "Wide panorama, low clouds over the ridgeline",
                &[
                    run(GPT, (21, 9), 2),
                    SeedRun(SEEDREAM, (21, 9), 4, Outcome::Failed(INSUFFICIENT)),
                ],
            ),
            SeedTurn(
                "Vertical phone wallpaper of the same lake",
                &[run(SEEDREAM, (9, 16), 4), run(QWEN, (9, 16), 2)],
            ),
            SeedTurn(
                "Drone shot gliding low over the lake toward the peaks",
                &[run(SEEDANCE, WIDE, 1)],
            ),
            SeedTurn(
                "Winter version with a frozen shoreline",
                &[run(GPT, (3, 2), 4)],
            ),
            SeedTurn("Moody blue-hour version", &[run(SD35, (2, 3), 4)]),
        ],
    },
    SeedThread {
        title: "Ceramic mug product shots",
        archived: false,
        unseen: false,
        updated_ago_min: 3 * 24 * 60,
        pace_min: 14,
        style: png::STYLE_BLOBS,
        palettes: [4, 3],
        turns: &[
            SeedTurn(
                "Speckled ceramic mug on linen, soft morning light, product photography",
                &[run(GPT, (4, 5), 4)],
            ),
            SeedTurn(
                "Same mug, hero shot on a terracotta backdrop",
                &[run(SEEDREAM, (4, 5), 4), run(QWEN, (4, 5), 2)],
            ),
            SeedTurn(
                "Flat lay with coffee beans and a brass spoon",
                &[run(GPT, SQUARE, 3)],
            ),
        ],
    },
    SeedThread {
        title: "Retro sci-fi paperback covers",
        archived: true,
        unseen: false,
        updated_ago_min: 16 * 24 * 60,
        pace_min: 20,
        style: png::STYLE_RINGS,
        palettes: [6, 2],
        turns: &[
            SeedTurn(
                "1970s sci-fi paperback cover, chrome spaceship over a red desert, airbrushed",
                &[run(SD35, (2, 3), 4), run(GPT, (2, 3), 4)],
            ),
            SeedTurn("Add a bold serif title block", &[run(GPT, (2, 3), 2)]),
        ],
    },
];

// ── state ──────────────────────────────────────────────────────────────────

struct Thread {
    id: StudioConversationId,
    title: String,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    archived: bool,
    forked_from_turn_id: Option<StudioTurnId>,
    last_seen_at: Option<DateTime<Utc>>,
    turns: Vec<StudioTurnView>,
}

#[derive(Clone)]
struct RunMeta {
    /// The first attempt's request (retries and "generate more" reuse it).
    request: GenerationRequest,
    updated_at: DateTime<Utc>,
    /// Painting recipe for the run's outputs.
    style: u32,
    palette: u32,
}

/// Where an artifact's pixels come from.
#[derive(Clone, Copy)]
struct Art {
    seed: u32,
    kind: MediaKind,
    width: u32,
    height: u32,
}

struct State {
    provider: StudioProviderConnection,
    secret: Option<String>,
    balance: f64,
    catalog: Vec<MediaModel>,
    fetched_at: DateTime<Utc>,
    threads: Vec<Thread>,
    runs: HashMap<StudioRunId, RunMeta>,
    art: HashMap<StudioArtifactId, Art>,
    variation: u32,
}

#[derive(Default)]
struct Media {
    previews: HashMap<StudioArtifactId, Arc<Vec<u8>>>,
    /// The few most recently read originals (they're megabytes each).
    originals: Vec<(StudioArtifactId, Arc<Vec<u8>>)>,
}

struct Prepared {
    model: MediaModel,
    request: GenerationRequest,
    quote: Option<Quote>,
}

pub(crate) struct DemoStudio {
    state: Mutex<State>,
    media: Mutex<Media>,
    /// Bumped on every write: the store's change channel.
    changes: tokio::sync::watch::Sender<u64>,
    cancel: CancellationToken,
    /// Divides every simulated duration (fast demo streams).
    speed: u32,
    rng: Mutex<u64>,
}

/// Engine timestamps are milliseconds; keep ours there too so cursors and
/// equality survive a JSON round trip.
fn ms(t: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(t.timestamp_millis()).unwrap_or(t)
}

fn now() -> DateTime<Utc> {
    ms(Utc::now())
}

fn failed(message: impl Into<String>) -> RpcError {
    RpcError::Failed(message.into())
}

fn invalid(message: impl std::fmt::Display) -> RpcError {
    RpcError::Failed(format!("invalid studio value: {message}"))
}

fn reply<T: Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| failed(format!("serialize response: {e}")))
}

impl DemoStudio {
    pub(crate) fn new(cancel: CancellationToken, fast: bool) -> Arc<Self> {
        let now = now();
        let fetched_at = now - chrono::Duration::minutes(40);
        let mut state = State {
            provider: StudioProviderConnection {
                provider_id: ProviderId::from(VENICE),
                display_label: "Venice".into(),
                configured: true,
                validation_state: ProviderValidationState::Valid,
                validated_at: Some(now - chrono::Duration::days(2)),
                validation_message: None,
                safe_mode: false,
            },
            secret: Some("demo-venice-key".into()),
            balance: 14.62,
            catalog: catalog(fetched_at),
            fetched_at,
            threads: Vec::new(),
            runs: HashMap::new(),
            art: HashMap::new(),
            variation: 0,
        };
        state.seed(now);
        let pending: Vec<StudioRunId> = state
            .threads
            .iter()
            .flat_map(|t| &t.turns)
            .flat_map(|t| &t.runs)
            .filter(|r| r.state.is_creating())
            .map(|r| r.id)
            .collect();
        let studio = Arc::new(Self {
            state: Mutex::new(state),
            media: Mutex::new(Media::default()),
            changes: tokio::sync::watch::channel(0).0,
            cancel,
            speed: if fast { 5 } else { 1 },
            rng: Mutex::new(0x51ed_270b_f3a1_9c47),
        });
        for run_id in pending {
            studio.play(run_id, true);
        }
        studio
    }

    fn notify(&self) {
        self.changes.send_modify(|v| *v += 1);
    }

    fn jitter(&self, lo: u64, hi: u64) -> u64 {
        let mut x = lock(&self.rng);
        *x ^= *x >> 12;
        *x ^= *x << 25;
        *x ^= *x >> 27;
        lo + x.wrapping_mul(0x2545_f491_4f6c_dd1d) % (hi - lo + 1)
    }

    fn pause(&self, ms: u64) -> Duration {
        Duration::from_millis(ms / u64::from(self.speed))
    }

    /// One unary Studio call.
    pub(crate) async fn rpc(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Value, RpcError> {
        let latency = match method {
            m::READ_STUDIO_PREVIEW_CHUNK | m::READ_STUDIO_ARTIFACT_CHUNK => 25,
            m::EVALUATE_STUDIO_COMPOSER => 45,
            m::VALIDATE_STUDIO_PROVIDER | m::GET_STUDIO_PROVIDER_BALANCE => 450,
            m::CREATE_STUDIO_TURN | m::EXTEND_STUDIO_TURN | m::RETRY_STUDIO_RUN => 220,
            _ => 90,
        };
        tokio::time::sleep(Duration::from_millis(latency)).await;
        match method {
            m::LIST_STUDIO_PROVIDERS => reply(&ListStudioProvidersResponse {
                providers: vec![lock(&self.state).provider.clone()],
            }),
            m::SET_STUDIO_PROVIDER_CREDENTIAL => {
                let request: SetStudioProviderCredentialRequest = zeron_rpc::parse_params(params)?;
                known_provider(&request.provider_id)?;
                let mut state = lock(&self.state);
                state.secret = Some(request.secret);
                state.provider = StudioProviderConnection {
                    provider_id: request.provider_id,
                    display_label: request.display_label,
                    configured: true,
                    validation_state: ProviderValidationState::NotValidated,
                    validated_at: None,
                    validation_message: None,
                    safe_mode: state.provider.safe_mode,
                };
                reply(&state.provider)
            }
            m::REMOVE_STUDIO_PROVIDER_CREDENTIAL => {
                let request: StudioProviderRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                if request.provider_id.as_str() == VENICE {
                    state.secret = None;
                    state.provider = StudioProviderConnection {
                        provider_id: request.provider_id,
                        display_label: "Venice".into(),
                        configured: false,
                        validation_state: ProviderValidationState::NotValidated,
                        validated_at: None,
                        validation_message: None,
                        safe_mode: false,
                    };
                }
                reply(&serde_json::json!({ "ok": true }))
            }
            m::SET_STUDIO_PROVIDER_PREFERENCES => {
                let request: SetStudioProviderPreferencesRequest = zeron_rpc::parse_params(params)?;
                known_provider(&request.provider_id)?;
                let mut state = lock(&self.state);
                state.provider.safe_mode = request.safe_mode;
                reply(&state.provider)
            }
            m::VALIDATE_STUDIO_PROVIDER => {
                let request: StudioProviderRequest = zeron_rpc::parse_params(params)?;
                known_provider(&request.provider_id)?;
                let mut state = lock(&self.state);
                let secret = state.secret.clone().ok_or_else(|| failed(NOT_CONFIGURED))?;
                // Anything that looks like a key is accepted; "bad…" isn't.
                let rejected = secret.trim().is_empty() || secret.starts_with("bad");
                let provider = &mut state.provider;
                if rejected {
                    provider.validation_state = ProviderValidationState::Invalid;
                    provider.validation_message = Some("Authentication failed".into());
                } else {
                    provider.validation_state = ProviderValidationState::Valid;
                    provider.validation_message = None;
                }
                provider.validated_at = Some(now());
                reply(&state.provider)
            }
            m::LIST_STUDIO_MODELS => {
                let request: ListStudioModelsRequest = zeron_rpc::parse_params(params)?;
                known_provider(&request.provider_id)?;
                if request.refresh {
                    if lock(&self.state).secret.is_none() {
                        return Err(failed(NOT_CONFIGURED));
                    }
                    tokio::time::sleep(Duration::from_millis(600)).await;
                    let mut state = lock(&self.state);
                    state.fetched_at = now();
                    state.catalog = catalog(state.fetched_at);
                }
                let state = lock(&self.state);
                let mut models = state.catalog.clone();
                if let Some(kind) = request.media_kind {
                    models.retain(|model| model.output_kind == kind);
                }
                models.retain(|model| model.is_picker_visible());
                reply(&ListStudioModelsResponse {
                    models,
                    fetched_at: state.fetched_at,
                    stale: false,
                })
            }
            m::EVALUATE_STUDIO_COMPOSER => {
                let request: EvaluateStudioComposerRequest = zeron_rpc::parse_params(params)?;
                let state = lock(&self.state);
                let catalog =
                    state.composer_catalog(&request.composer, request.provider_id.as_ref())?;
                reply(&zeron_studio::evaluate_composer(
                    &request.composer,
                    &catalog,
                ))
            }
            m::IMPORT_STUDIO_ASSET => {
                Err(failed("the demo desktop can't import Studio attachments"))
            }
            m::LIST_STUDIO_CONVERSATIONS => {
                let request: ListStudioConversationsRequest = zeron_rpc::parse_params(params)?;
                reply(&ListStudioConversationsResponse {
                    conversations: lock(&self.state).conversations(request.include_archived),
                })
            }
            m::LIST_STUDIO_ARTIFACTS => {
                let request: ListStudioArtifactsRequest = zeron_rpc::parse_params(params)?;
                reply(&lock(&self.state).gallery_page(request.limit, request.cursor.as_ref()))
            }
            m::CREATE_STUDIO_CONVERSATION => {
                let request: CreateStudioConversationRequest = zeron_rpc::parse_params(params)?;
                let title = validate_title(&request.title)?;
                let now = now();
                let id = StudioConversationId::new();
                let mut state = lock(&self.state);
                state.threads.push(Thread {
                    id,
                    title: title.to_owned(),
                    created_at: now,
                    updated_at: now,
                    archived: false,
                    forked_from_turn_id: request.forked_from_turn_id,
                    last_seen_at: Some(now),
                    turns: Vec::new(),
                });
                let summary = state.summary(state.thread(id)?);
                drop(state);
                self.notify();
                reply(&summary)
            }
            m::RENAME_STUDIO_CONVERSATION => {
                let request: RenameStudioConversationRequest = zeron_rpc::parse_params(params)?;
                let title = validate_title(&request.title)?;
                self.update_thread(request.conversation_id, |thread| {
                    thread.title = title.to_owned();
                    thread.updated_at = now();
                })
            }
            m::ARCHIVE_STUDIO_CONVERSATION => {
                let request: ArchiveStudioConversationRequest = zeron_rpc::parse_params(params)?;
                self.update_thread(request.conversation_id, |thread| {
                    thread.archived = request.archived;
                    thread.updated_at = now();
                })
            }
            m::DELETE_STUDIO_CONVERSATION => {
                let request: DeleteStudioConversationRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                let index = state
                    .threads
                    .iter()
                    .position(|t| t.id == request.conversation_id)
                    .ok_or_else(|| failed(CONVERSATION_NOT_FOUND))?;
                let thread = state.threads.remove(index);
                for run in thread.turns.iter().flat_map(|t| &t.runs) {
                    state.runs.remove(&run.id);
                    for artifact in &run.artifacts {
                        state.art.remove(&artifact.id);
                    }
                }
                drop(state);
                self.notify();
                reply(&serde_json::json!({ "ok": true }))
            }
            m::MARK_STUDIO_CONVERSATION_SEEN => {
                let request: MarkStudioConversationSeenRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                let thread = state.thread(request.conversation_id)?;
                let stale = thread
                    .last_seen_at
                    .is_none_or(|seen| state.settled_since(thread, seen));
                if stale {
                    state.thread_mut(request.conversation_id)?.last_seen_at = Some(now());
                }
                let summary = state.summary(state.thread(request.conversation_id)?);
                drop(state);
                if stale {
                    self.notify();
                }
                reply(&summary)
            }
            m::QUOTE_STUDIO_BATCH => {
                let request: QuoteStudioBatchRequest = zeron_rpc::parse_params(params)?;
                let state = lock(&self.state);
                let prepared = match request.composer.as_ref() {
                    Some(composer) => state.prepare_composer(&request.prompt, composer, false)?,
                    None => state.prepare(&request.prompt, request.runs, false)?,
                };
                let runs: Vec<QuoteStudioRunView> = prepared
                    .into_iter()
                    .map(|run| QuoteStudioRunView {
                        provider_id: run.request.provider_id,
                        model_id: run.request.model_id,
                        quote: run.quote,
                    })
                    .collect();
                let total = runs
                    .iter()
                    .map(|run| run.quote.clone())
                    .collect::<Option<Vec<_>>>()
                    .and_then(Quote::total);
                reply(&QuoteStudioBatchResponse { runs, total })
            }
            m::GET_STUDIO_PROVIDER_BALANCE => {
                let request: StudioProviderRequest = zeron_rpc::parse_params(params)?;
                known_provider(&request.provider_id)?;
                let state = lock(&self.state);
                if state.secret.is_none() {
                    return Err(failed(NOT_CONFIGURED));
                }
                reply(&StudioProviderBalanceResponse {
                    balance: Some(AccountBalance {
                        remaining: Quote::provider("USD", (state.balance * 100.0).round() / 100.0),
                    }),
                })
            }
            m::CREATE_STUDIO_TURN => {
                let request: CreateStudioTurnRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                let prepared = match request.composer.as_ref() {
                    Some(composer) => state.prepare_composer(&request.prompt, composer, true)?,
                    None => {
                        if request
                            .runs
                            .iter()
                            .any(|run| run.operation == MediaOperation::ImageEdit)
                        {
                            return Err(failed(
                                "image edits must be appended to the source image's turn",
                            ));
                        }
                        if request.runs.iter().any(|run| is_video(run.operation)) {
                            return Err(failed("video runs require a composer snapshot"));
                        }
                        state.prepare(&request.prompt, request.runs, true)?
                    }
                };
                let runs = state.create_turn(
                    request.conversation_id,
                    &request.prompt,
                    request.source_turn_id,
                    prepared,
                )?;
                let view = state.view(request.conversation_id)?;
                drop(state);
                self.notify();
                for run_id in runs {
                    self.play(run_id, false);
                }
                reply(&view)
            }
            m::EXTEND_STUDIO_TURN => {
                let request: ExtendStudioTurnRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                let (prompt, specs) = state.extend_spec(request.turn_id)?;
                if specs.iter().any(|run| is_video(run.operation)) {
                    return Err(failed("video extend requires a composer snapshot"));
                }
                let prepared = state.prepare(&prompt, specs, true)?;
                let (conversation_id, runs) = state.extend_turn(request.turn_id, prepared)?;
                let view = state.view(conversation_id)?;
                drop(state);
                self.notify();
                for run_id in runs {
                    self.play(run_id, false);
                }
                reply(&view)
            }
            m::APPEND_STUDIO_DERIVED_RUN => {
                let mut request: AppendStudioDerivedRunRequest = zeron_rpc::parse_params(params)?;
                if !matches!(
                    request.run.operation,
                    MediaOperation::ImageEdit | MediaOperation::Upscale
                ) {
                    return Err(failed("derived runs must be image-edit or upscale"));
                }
                if request.run.output_count != 1 {
                    return Err(failed("derived runs produce exactly one image"));
                }
                if request.run.operation == MediaOperation::ImageEdit
                    && request.prompt.trim().is_empty()
                {
                    return Err(failed("image edit requires a prompt"));
                }
                if !request
                    .run
                    .inputs
                    .iter()
                    .any(|i| i.role.as_str() == ROLE_SOURCE)
                {
                    request.run.inputs.insert(
                        0,
                        GenerationInput {
                            role: ROLE_SOURCE.into(),
                            ordinal: 0,
                            source: GenerationInputSource::Artifact {
                                artifact_id: request.source_artifact_id,
                            },
                            content_hash: String::new(),
                        },
                    );
                }
                let mut state = lock(&self.state);
                let (ti, ui, _, _) = state
                    .locate_artifact(request.source_artifact_id)
                    .ok_or_else(|| failed(ARTIFACT_NOT_FOUND))?;
                let turn_id = state.threads[ti].turns[ui].id;
                // The demo catalog has no edit or upscale models, so this
                // answers like an engine whose catalog lacks the model.
                let prepared = state.prepare(&request.prompt, vec![request.run], true)?;
                let (conversation_id, runs) = state.extend_turn(turn_id, prepared)?;
                let view = state.view(conversation_id)?;
                drop(state);
                self.notify();
                for run_id in runs {
                    self.play(run_id, false);
                }
                reply(&view)
            }
            m::DELETE_STUDIO_ARTIFACT => {
                let request: DeleteStudioArtifactRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                let (ti, ui, ri, ai) = state
                    .locate_artifact(request.artifact_id)
                    .ok_or_else(|| failed(ARTIFACT_NOT_FOUND))?;
                state.threads[ti].turns[ui].runs[ri].artifacts.remove(ai);
                state.art.remove(&request.artifact_id);
                drop(state);
                let mut media = lock(&self.media);
                media.previews.remove(&request.artifact_id);
                media.originals.retain(|(id, _)| *id != request.artifact_id);
                drop(media);
                self.notify();
                reply(&serde_json::json!({ "ok": true }))
            }
            m::RETRY_STUDIO_RUN => {
                let request: RetryStudioRunRequest = zeron_rpc::parse_params(params)?;
                let mut state = lock(&self.state);
                let (ti, ui, ri) = state
                    .locate_run(request.run_id)
                    .ok_or_else(|| failed(RUN_NOT_FOUND))?;
                let conversation_id = state.threads[ti].id;
                let run = &mut state.threads[ti].turns[ui].runs[ri];
                match run.state {
                    StudioRunState::Queued
                    | StudioRunState::Running
                    | StudioRunState::Downloading => {
                        return Err(invalid(
                            "cannot retry a run that is still queued, running, or downloading",
                        ));
                    }
                    StudioRunState::Failed | StudioRunState::Cancelled => {}
                    _ => {
                        return Err(invalid(
                            "only terminal failed or uncertain runs can be retried",
                        ));
                    }
                }
                run.state = StudioRunState::Queued;
                run.progress = None;
                run.error = None;
                if let Some(meta) = state.runs.get_mut(&request.run_id) {
                    meta.updated_at = now();
                }
                let view = state.view(conversation_id)?;
                drop(state);
                self.notify();
                self.play(request.run_id, false);
                reply(&view)
            }
            m::CHECK_STUDIO_RUN => {
                // The engine re-polls the provider in the background; demo
                // runs already report everything they know.
                let request: CheckStudioRunRequest = zeron_rpc::parse_params(params)?;
                let state = lock(&self.state);
                let (ti, ..) = state
                    .locate_run(request.run_id)
                    .ok_or_else(|| failed(RUN_NOT_FOUND))?;
                reply(&state.view(state.threads[ti].id)?)
            }
            m::READ_STUDIO_PREVIEW_CHUNK => {
                let request: ReadStudioArtifactChunkRequest = zeron_rpc::parse_params(params)?;
                let bytes = self.preview(request.artifact_id)?;
                reply(&chunk(
                    request.artifact_id,
                    &bytes,
                    request.offset,
                    "jpg",
                    "image/jpeg",
                ))
            }
            m::READ_STUDIO_ARTIFACT_CHUNK => {
                let request: ReadStudioArtifactChunkRequest = zeron_rpc::parse_params(params)?;
                let bytes = self.original(request.artifact_id)?;
                reply(&chunk(
                    request.artifact_id,
                    &bytes,
                    request.offset,
                    "png",
                    "image/png",
                ))
            }
            other => Err(RpcError::UnknownMethod(other.to_owned())),
        }
    }

    fn update_thread(
        &self,
        id: StudioConversationId,
        f: impl FnOnce(&mut Thread),
    ) -> Result<Value, RpcError> {
        let mut state = lock(&self.state);
        f(state.thread_mut(id)?);
        let summary = state.summary(state.thread(id)?);
        drop(state);
        self.notify();
        reply(&summary)
    }

    /// Follow a `WatchStudio*` stream: the current state at once, then a
    /// fresh snapshot after every change, until the receiver is dropped.
    pub(crate) fn subscribe(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<tokio::sync::mpsc::Receiver<Value>, RpcError> {
        let render: Box<dyn Fn(&State) -> Option<Value> + Send> = match method {
            m::WATCH_STUDIO_CONVERSATIONS => {
                let request: ListStudioConversationsRequest = zeron_rpc::parse_params(params)?;
                Box::new(move |state| {
                    serde_json::to_value(ListStudioConversationsResponse {
                        conversations: state.conversations(request.include_archived),
                    })
                    .ok()
                })
            }
            m::WATCH_STUDIO_GALLERY => {
                let request: ListStudioArtifactsRequest = zeron_rpc::parse_params(params)?;
                Box::new(move |state| {
                    serde_json::to_value(state.gallery_page(request.limit, request.cursor.as_ref()))
                        .ok()
                })
            }
            m::WATCH_STUDIO_CONVERSATION => {
                let request: WatchStudioConversationRequest = zeron_rpc::parse_params(params)?;
                // Fail the subscription synchronously for an unknown thread;
                // a deleted one ends the stream.
                lock(&self.state).view(request.conversation_id)?;
                Box::new(move |state| {
                    let view = state.view(request.conversation_id).ok()?;
                    serde_json::to_value(view).ok()
                })
            }
            other => return Err(RpcError::UnknownMethod(other.to_owned())),
        };
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let mut changes = self.changes.subscribe();
        let studio = self.clone();
        crate::runtime::shared().spawn(async move {
            loop {
                let value = {
                    let state = lock(&studio.state);
                    render(&state)
                };
                let Some(value) = value else {
                    return;
                };
                if tx.send(value).await.is_err() {
                    return;
                }
                tokio::select! {
                    _ = studio.cancel.cancelled() => return,
                    _ = tx.closed() => return,
                    changed = changes.changed() => if changed.is_err() { return },
                }
            }
        });
        Ok(rx)
    }

    // ── runs ───────────────────────────────────────────────────────────────

    /// Play a queued run to its end on the host's clock. `seeded` runs (the
    /// one already generating when Studio opens) crawl, so it stays visibly
    /// in flight for a while.
    fn play(self: &Arc<Self>, run_id: StudioRunId, seeded: bool) {
        let studio = self.clone();
        crate::runtime::shared().spawn(async move {
            let Some((video, doomed, mut progress)) = studio.run_outlook(run_id) else {
                return;
            };
            let wait = |ms: u64| {
                let pause = studio.pause(ms);
                let cancel = studio.cancel.clone();
                async move {
                    tokio::select! {
                        _ = cancel.cancelled() => false,
                        _ = tokio::time::sleep(pause) => true,
                    }
                }
            };
            if doomed {
                // Venice refuses at submit: no progress, just the error.
                if !wait(studio.jitter(900, 1500)).await {
                    return;
                }
            } else if video {
                let (tick, step) = if seeded { (2500, 0.03) } else { (650, 0.12) };
                if progress.is_none() && !wait(1200).await {
                    return;
                }
                loop {
                    let next = progress.unwrap_or(0.0) + step;
                    if next >= 0.92 {
                        break;
                    }
                    progress = Some(next);
                    if !studio.advance(run_id, StudioRunState::Running, progress)
                        || !wait(tick).await
                    {
                        return;
                    }
                }
                if !studio.advance(run_id, StudioRunState::Downloading, None) || !wait(700).await {
                    return;
                }
            } else if !wait(studio.jitter(2000, 4000)).await {
                return;
            }
            studio.finish(run_id);
        });
    }

    /// (video?, will it fail?, current progress) for a run still in flight.
    fn run_outlook(&self, run_id: StudioRunId) -> Option<(bool, bool, Option<f32>)> {
        let state = lock(&self.state);
        let (ti, ui, ri) = state.locate_run(run_id)?;
        let run = &state.threads[ti].turns[ui].runs[ri];
        let meta = state.runs.get(&run_id)?;
        let doomed = state.secret.is_none() || prompt_fails(&meta.request.prompt);
        Some((is_video(meta.request.operation), doomed, run.progress))
    }

    /// Move an in-flight run to `run_state`; false once it's gone or settled.
    fn advance(
        &self,
        run_id: StudioRunId,
        run_state: StudioRunState,
        progress: Option<f32>,
    ) -> bool {
        let mut state = lock(&self.state);
        let Some((ti, ui, ri)) = state.locate_run(run_id) else {
            return false;
        };
        let run = &mut state.threads[ti].turns[ui].runs[ri];
        if !run.state.is_creating() {
            return false;
        }
        run.state = run_state;
        run.progress = progress;
        if let Some(meta) = state.runs.get_mut(&run_id) {
            meta.updated_at = now();
        }
        drop(state);
        self.notify();
        true
    }

    /// Land a run: its artifacts, or the provider's refusal.
    fn finish(&self, run_id: StudioRunId) {
        let mut state = lock(&self.state);
        let Some((ti, ui, ri)) = state.locate_run(run_id) else {
            return;
        };
        let Some(meta) = state.runs.get(&run_id).cloned() else {
            return;
        };
        if !state.threads[ti].turns[ui].runs[ri].state.is_creating() {
            return;
        }
        let at = now();
        let failure = if state.secret.is_none() {
            Some(NOT_CONFIGURED)
        } else if prompt_fails(&meta.request.prompt) {
            Some(INSUFFICIENT)
        } else {
            None
        };
        let artifacts = match failure {
            Some(_) => Vec::new(),
            None => state.outputs(&meta, at),
        };
        let spent = state.threads[ti].turns[ui].runs[ri]
            .quote
            .as_ref()
            .filter(|_| failure.is_none())
            .map_or(0.0, |quote| quote.amount);
        state.balance = (state.balance - spent).max(0.0);
        let run = &mut state.threads[ti].turns[ui].runs[ri];
        run.progress = None;
        match failure {
            Some(message) => {
                run.state = StudioRunState::Failed;
                run.error = Some(message.to_owned());
            }
            None => {
                run.state = StudioRunState::Succeeded;
                run.error = None;
                run.artifacts.extend(artifacts);
            }
        }
        if let Some(meta) = state.runs.get_mut(&run_id) {
            meta.updated_at = at;
        }
        drop(state);
        self.notify();
    }

    // ── media ──────────────────────────────────────────────────────────────

    fn art(&self, id: StudioArtifactId) -> Result<Art, RpcError> {
        lock(&self.state)
            .art
            .get(&id)
            .copied()
            .ok_or_else(|| failed(ARTIFACT_NOT_FOUND))
    }

    /// The gallery JPEG (a video's poster), derived like the engine's.
    fn preview(&self, id: StudioArtifactId) -> Result<Arc<Vec<u8>>, RpcError> {
        let art = self.art(id)?;
        if let Some(bytes) = lock(&self.media).previews.get(&id) {
            return Ok(bytes.clone());
        }
        let (w, h) = thumb_dimensions(art.width, art.height);
        let rgb = png::art(w, h, art.seed);
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 82)
            .encode(&rgb, w, h, image::ExtendedColorType::Rgb8)
            .map_err(|e| failed(format!("studio preview: {e}")))?;
        let bytes = Arc::new(jpeg);
        lock(&self.media).previews.insert(id, bytes.clone());
        Ok(bytes)
    }

    fn original(&self, id: StudioArtifactId) -> Result<Arc<Vec<u8>>, RpcError> {
        let art = self.art(id)?;
        if art.kind == MediaKind::Video {
            return Err(failed(
                "the demo desktop has no video originals; play the poster instead",
            ));
        }
        if let Some((_, bytes)) = lock(&self.media).originals.iter().find(|(a, _)| *a == id) {
            return Ok(bytes.clone());
        }
        let rgb = png::art(art.width, art.height, art.seed);
        let bytes = Arc::new(png::encode_rgb(art.width, art.height, &rgb));
        let mut media = lock(&self.media);
        media.originals.push((id, bytes.clone()));
        if media.originals.len() > 3 {
            media.originals.remove(0);
        }
        Ok(bytes)
    }
}

fn known_provider(id: &ProviderId) -> Result<(), RpcError> {
    if id.as_str() == VENICE {
        Ok(())
    } else {
        Err(failed("unknown studio provider"))
    }
}

fn validate_title(title: &str) -> Result<&str, RpcError> {
    let title = title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(invalid(
            "conversation title must contain 1 to 200 characters",
        ));
    }
    Ok(title)
}

fn title_from_prompt(prompt: &str) -> Option<String> {
    let title: String = prompt
        .split_whitespace()
        .take(7)
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(48)
        .collect();
    let title = title.trim().to_string();
    (!title.is_empty()).then_some(title)
}

fn is_video(operation: MediaOperation) -> bool {
    matches!(
        operation,
        MediaOperation::TextToVideo
            | MediaOperation::ImageToVideo
            | MediaOperation::ReferenceToVideo
            | MediaOperation::VideoToVideo
    )
}

fn submit_operation_allowed(operation: MediaOperation) -> bool {
    is_video(operation)
        || matches!(
            operation,
            MediaOperation::TextToImage | MediaOperation::Upscale | MediaOperation::ImageEdit
        )
}

/// The demo's way to see a failed run: say "fail".
fn prompt_fails(prompt: &str) -> bool {
    prompt
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| word.eq_ignore_ascii_case("fail"))
}

/// The engine's `studio_validation_error`: the blocking conflict's title, with
/// the structured payload alongside.
fn validation_error(conflicts: &[ComposerConflict]) -> RpcError {
    let message = conflicts
        .iter()
        .find(|conflict| conflict.blocks_send())
        .or(conflicts.first())
        .map(|conflict| conflict.title.clone())
        .unwrap_or_else(|| "studio validation failed".into());
    let payload =
        serde_json::to_value(StudioValidationError::new(conflicts.to_vec())).unwrap_or_default();
    RpcError::FailedStructured { message, payload }
}

/// Model defaults under explicit `controls`, as the composer projects them
/// (duration is the composer's global, never a default).
fn with_defaults(
    model: &MediaModel,
    mut controls: BTreeMap<ControlId, ControlValue>,
) -> BTreeMap<ControlId, ControlValue> {
    for control in &model.controls {
        if control.id.as_str() == "duration" {
            continue;
        }
        if !controls.contains_key(&control.id)
            && let Some(default) = &control.default
        {
            controls.insert(control.id.clone(), default.clone());
        }
    }
    controls
}

/// The engine's `project_composer_specs`: one run spec per selected model.
fn project_specs(
    snapshot: &ComposerSnapshot,
    catalog: &[MediaModel],
) -> Result<Vec<StudioModelRunSpec>, RpcError> {
    let mut specs = Vec::with_capacity(snapshot.selected.len());
    for selected in &snapshot.selected {
        let model = catalog
            .iter()
            .find(|model| {
                model.id == selected.model_id && model.provider_id == selected.provider_id
            })
            .ok_or_else(|| failed("studio model is unavailable"))?;
        let inputs = zeron_studio::map_tray(snapshot, model)
            .map_err(|conflict| validation_error(std::slice::from_ref(&conflict)))?;
        let mut controls = with_defaults(model, selected.controls.clone());
        if snapshot.mode == ComposerMode::Video
            && let Some(duration) = zeron_studio::assigned_duration(snapshot, model)
        {
            controls.insert(ControlId::from("duration"), duration);
        }
        let output_count = match snapshot.mode {
            ComposerMode::Video => 1,
            ComposerMode::Image => selected.output_count.max(1),
        };
        specs.push(StudioModelRunSpec {
            provider_id: selected.provider_id.clone(),
            model_id: selected.model_id.clone(),
            operation: model.operation,
            output_count,
            display_aspect_ratio: display_aspect(snapshot, &controls, &inputs),
            controls,
            inputs,
            manifest_version: model.manifest_version.clone(),
        });
    }
    Ok(specs)
}

/// The engine's `composer_display_aspect`.
fn display_aspect(
    snapshot: &ComposerSnapshot,
    controls: &BTreeMap<ControlId, ControlValue>,
    inputs: &[GenerationInput],
) -> (u32, u32) {
    if let Some((width, height)) = controls.values().find_map(|v| v.aspect_ratio_dimensions())
        && width > 0
        && height > 0
    {
        return (width, height);
    }
    for input in inputs.iter().filter(|i| i.role.as_str() == ROLE_SOURCE) {
        let attachment = snapshot.attachments.iter().find(|a| match &input.source {
            GenerationInputSource::Asset { asset_id } => a.id == *asset_id,
            GenerationInputSource::Artifact { artifact_id } => matches!(
                &a.origin,
                zeron_studio::AttachmentOrigin::Artifact { artifact_id: origin }
                    if origin == artifact_id
            ),
        });
        if let Some(a) = attachment
            && let (Some(width), Some(height)) = (a.width, a.height)
            && width > 0
            && height > 0
        {
            return (width, height);
        }
    }
    (16, 9)
}

fn aspect_of(controls: &BTreeMap<ControlId, ControlValue>, fallback: (u32, u32)) -> (u32, u32) {
    controls
        .get(&ControlId::from("aspect_ratio"))
        .and_then(ControlValue::aspect_ratio_dimensions)
        .filter(|(w, h)| *w > 0 && *h > 0)
        .unwrap_or(fallback)
}

/// Pixel size at `long` along the longer side.
fn fit(aspect: (u32, u32), long: u32) -> (u32, u32) {
    let (w, h) = (aspect.0.max(1) as f64, aspect.1.max(1) as f64);
    if w >= h {
        (long, (long as f64 * h / w).round().max(1.0) as u32)
    } else {
        ((long as f64 * w / h).round().max(1.0) as u32, long)
    }
}

/// `720p` at `aspect`: the short side is the resolution.
fn video_dimensions(aspect: (u32, u32), resolution: &str) -> (u32, u32) {
    let short: u32 = resolution.trim_end_matches('p').parse().unwrap_or(720);
    let (w, h) = (aspect.0.max(1) as f64, aspect.1.max(1) as f64);
    let long = (short as f64 * w.max(h) / w.min(h)).round() as u32;
    if w >= h { (long, short) } else { (short, long) }
}

/// The engine's `gallery_thumb_dimensions`.
fn thumb_dimensions(width: u32, height: u32) -> (u32, u32) {
    let short = width.min(height);
    let long = width.max(height);
    if short <= THUMB_SHORT_EDGE && long <= THUMB_LONG_EDGE {
        return (width, height);
    }
    let scale = (THUMB_SHORT_EDGE as f64 / short.max(1) as f64)
        .min(THUMB_LONG_EDGE as f64 / long.max(1) as f64)
        .min(1.0);
    (
        (width as f64 * scale).round().max(1.0) as u32,
        (height as f64 * scale).round().max(1.0) as u32,
    )
}

/// ThumbHash of the painting, from a ≤100px copy (the official encoder's
/// limit), standard base64 — what the engine stores beside each preview.
fn thumbhash_of(art: &Art) -> String {
    let scale = 100.0 / art.width.max(art.height).max(1) as f64;
    let w = ((art.width as f64 * scale).round() as u32).clamp(1, 100);
    let h = ((art.height as f64 * scale).round() as u32).clamp(1, 100);
    let rgb = png::art(w, h, art.seed);
    let rgba: Vec<u8> = rgb
        .chunks(3)
        .flat_map(|p| [p[0], p[1], p[2], 255])
        .collect();
    BASE64.encode(thumbhash::rgba_to_thumb_hash(w as usize, h as usize, &rgba))
}

/// One base64 slice of `bytes` at `offset`, like the engine's chunk readers.
fn chunk(
    id: StudioArtifactId,
    bytes: &[u8],
    offset: u64,
    extension: &str,
    mime: &str,
) -> StudioArtifactChunk {
    let size = bytes.len();
    let start = (offset as usize).min(size);
    let next = (start + CHUNK_BYTES).min(size);
    StudioArtifactChunk {
        artifact_id: id,
        file_name: format!("{}.{extension}", id.0),
        mime_type: mime.to_owned(),
        data: BASE64.encode(&bytes[start..next]),
        next_offset: next as u64,
        done: next >= size,
    }
}

/// Painting style and palette for a new prompt, by what it talks about.
fn recipe_for(prompt: &str) -> (u32, u32) {
    let p = prompt.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| p.contains(w));
    let style = if has(&[
        "lake",
        "mountain",
        "landscape",
        "valley",
        "forest",
        "ocean",
        "sea",
        "desert",
        "hill",
        "drone",
        "field",
        "beach",
    ]) {
        png::STYLE_LANDSCAPE
    } else if has(&[
        "retro",
        "poster",
        "cover",
        "psychedelic",
        "vinyl",
        "pattern",
        "70s",
        "80s",
    ]) {
        png::STYLE_RINGS
    } else if has(&[
        "isometric",
        "room",
        "interior",
        "architecture",
        "building",
        "product",
        "grid",
        "city",
        "house",
    ]) {
        png::STYLE_TILES
    } else {
        png::STYLE_BLOBS
    };
    let palette = if has(&["neon", "night", "cyber"]) {
        2
    } else if has(&["golden", "sunset", "warm"]) {
        0
    } else if has(&["winter", "snow", "ice", "alpine"]) {
        1
    } else if has(&["blue hour", "moody", "dusk"]) {
        5
    } else if has(&["desert", "mars", "red"]) {
        6
    } else if has(&["fog", "concrete", "minimal"]) {
        7
    } else if has(&["plant", "clay", "garden", "green"]) {
        3
    } else if has(&["terracotta", "mug", "ceramic"]) {
        4
    } else {
        prompt
            .bytes()
            .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32))
            % 8
    };
    (style, palette)
}

impl State {
    fn thread(&self, id: StudioConversationId) -> Result<&Thread, RpcError> {
        self.threads
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| failed(CONVERSATION_NOT_FOUND))
    }

    fn thread_mut(&mut self, id: StudioConversationId) -> Result<&mut Thread, RpcError> {
        self.threads
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| failed(CONVERSATION_NOT_FOUND))
    }

    fn locate_run(&self, run_id: StudioRunId) -> Option<(usize, usize, usize)> {
        self.threads.iter().enumerate().find_map(|(ti, thread)| {
            thread.turns.iter().enumerate().find_map(|(ui, turn)| {
                turn.runs
                    .iter()
                    .position(|run| run.id == run_id)
                    .map(|ri| (ti, ui, ri))
            })
        })
    }

    fn locate_artifact(&self, id: StudioArtifactId) -> Option<(usize, usize, usize, usize)> {
        self.threads.iter().enumerate().find_map(|(ti, thread)| {
            thread.turns.iter().enumerate().find_map(|(ui, turn)| {
                turn.runs.iter().enumerate().find_map(|(ri, run)| {
                    run.artifacts
                        .iter()
                        .position(|a| a.id == id)
                        .map(|ai| (ti, ui, ri, ai))
                })
            })
        })
    }

    /// A run settled after `seen` — the store's "done" test.
    fn settled_since(&self, thread: &Thread, seen: DateTime<Utc>) -> bool {
        thread.turns.iter().flat_map(|t| &t.runs).any(|run| {
            !run.state.is_creating()
                && self
                    .runs
                    .get(&run.id)
                    .is_some_and(|meta| meta.updated_at > seen)
        })
    }

    fn summary(&self, thread: &Thread) -> StudioConversationSummary {
        StudioConversationSummary {
            id: thread.id,
            title: thread.title.clone(),
            turn_count: thread.turns.len() as u32,
            created_at: thread.created_at,
            updated_at: thread.updated_at,
            archived: thread.archived,
            forked_from_turn_id: thread.forked_from_turn_id,
            creating: thread
                .turns
                .iter()
                .flat_map(|t| &t.runs)
                .any(|run| run.state.is_creating()),
            done: thread
                .last_seen_at
                .is_some_and(|seen| self.settled_since(thread, seen)),
        }
    }

    fn conversations(&self, include_archived: bool) -> Vec<StudioConversationSummary> {
        let mut list: Vec<StudioConversationSummary> = self
            .threads
            .iter()
            .filter(|t| include_archived || !t.archived)
            .map(|t| self.summary(t))
            .collect();
        list.sort_by(|a, b| (b.updated_at, b.id).cmp(&(a.updated_at, a.id)));
        list
    }

    fn view(&self, id: StudioConversationId) -> Result<StudioConversationView, RpcError> {
        let thread = self.thread(id)?;
        Ok(StudioConversationView {
            conversation: self.summary(thread),
            turns: thread.turns.clone(),
        })
    }

    /// The store's `list_gallery_page`: newest first by (createdAt, id), a
    /// cursor strictly after the last item, `limit` clamped to 1…200.
    fn gallery_page(
        &self,
        limit: Option<u32>,
        cursor: Option<&StudioGalleryCursor>,
    ) -> ListStudioArtifactsResponse {
        let mut items: Vec<StudioGalleryItem> = Vec::new();
        for thread in &self.threads {
            for turn in &thread.turns {
                for run in &turn.runs {
                    let source = run.inputs.iter().find_map(|input| match &input.source {
                        GenerationInputSource::Artifact { artifact_id }
                            if input.role.as_str() == ROLE_SOURCE && input.ordinal == 0 =>
                        {
                            Some(*artifact_id)
                        }
                        _ => None,
                    });
                    for artifact in &run.artifacts {
                        items.push(StudioGalleryItem {
                            id: artifact.id,
                            conversation_id: thread.id,
                            turn_id: turn.id,
                            output_position: artifact.output_position,
                            media_kind: artifact.media_kind,
                            mime_type: artifact.mime_type.clone(),
                            size_bytes: artifact.size_bytes,
                            width: artifact.width,
                            height: artifact.height,
                            prompt: run.prompt.clone().unwrap_or_else(|| turn.prompt.clone()),
                            model_display_name: run.model.display_name.clone(),
                            created_at: artifact.created_at,
                            thumbhash: artifact.thumbhash.clone(),
                            source_artifact_id: source,
                            duration_seconds: artifact.duration_seconds,
                        });
                    }
                }
            }
        }
        if let Some(cursor) = cursor {
            let at = cursor.created_at.timestamp_millis();
            items.retain(|item| {
                let t = item.created_at.timestamp_millis();
                t < at || (t == at && item.id < cursor.artifact_id)
            });
        }
        items.sort_by(|a, b| {
            (b.created_at.timestamp_millis(), b.id).cmp(&(a.created_at.timestamp_millis(), a.id))
        });
        let limit = limit.map(|value| value.clamp(1, 200) as usize);
        let has_more = limit.is_some_and(|limit| items.len() > limit);
        if let Some(limit) = limit {
            items.truncate(limit);
        }
        let next_cursor =
            has_more
                .then(|| items.last())
                .flatten()
                .map(|last| StudioGalleryCursor {
                    created_at: last.created_at,
                    artifact_id: last.id,
                });
        ListStudioArtifactsResponse {
            artifacts: items,
            next_cursor,
        }
    }

    /// The engine's `composer_catalog`: every model of every provider the
    /// composer touches.
    fn composer_catalog(
        &self,
        snapshot: &ComposerSnapshot,
        provider_id: Option<&ProviderId>,
    ) -> Result<Vec<MediaModel>, RpcError> {
        let mut touched = provider_id
            .into_iter()
            .chain(snapshot.selected.iter().map(|s| &s.provider_id));
        if let Some(unknown) = touched.find(|id| id.as_str() != VENICE) {
            known_provider(unknown)?;
        }
        Ok(self.catalog.clone())
    }

    fn prepare_composer(
        &self,
        prompt: &str,
        composer: &ComposerSnapshot,
        submit: bool,
    ) -> Result<Vec<Prepared>, RpcError> {
        if prompt != composer.prompt {
            return Err(RpcError::BadParams(
                "prompt must equal composer.prompt".into(),
            ));
        }
        let catalog = self.composer_catalog(composer, None)?;
        let view = zeron_studio::evaluate_composer(composer, &catalog);
        if !view.send.enabled || view.conflicts.iter().any(|c| c.blocks_send()) {
            return Err(validation_error(&view.conflicts));
        }
        let specs = project_specs(composer, &catalog)?;
        self.prepare(prompt, specs, submit)
    }

    /// The engine's `prepare_studio_runs`: bind each spec to its catalog
    /// model and price it.
    fn prepare(
        &self,
        prompt: &str,
        specs: Vec<StudioModelRunSpec>,
        submit: bool,
    ) -> Result<Vec<Prepared>, RpcError> {
        let mut prepared = Vec::with_capacity(specs.len());
        for spec in specs {
            if submit && !submit_operation_allowed(spec.operation) {
                return Err(failed(
                    "only text-to-image, image-edit, upscale, and video runs are available in the current Studio slice",
                ));
            }
            known_provider(&spec.provider_id)?;
            let model = self
                .catalog
                .iter()
                .find(|model| model.id == spec.model_id)
                .cloned()
                .ok_or_else(|| failed("studio model is unavailable"))?;
            let mut request = GenerationRequest {
                provider_id: spec.provider_id,
                model_id: spec.model_id,
                operation: spec.operation,
                prompt: prompt.to_owned(),
                negative_prompt: None,
                output_count: spec.output_count,
                controls: spec.controls,
                inputs: spec.inputs,
                manifest_version: spec.manifest_version,
                display_aspect_ratio: spec.display_aspect_ratio,
            };
            self.bind_inputs(&mut request)?;
            request.drop_unknown_controls(&model);
            request.bind_to(&model).map_err(|e| failed(e.to_string()))?;
            if submit && model.controls.iter().any(|c| c.id.as_str() == "safe_mode") {
                request.controls.insert(
                    ControlId::from("safe_mode"),
                    ControlValue::Boolean {
                        value: self.provider.safe_mode,
                    },
                );
            }
            let quote = model.estimate_cost(&request.controls, request.output_count);
            prepared.push(Prepared {
                model,
                request,
                quote,
            });
        }
        Ok(prepared)
    }

    /// The store's `bind_generation_inputs`, for the inputs a demo can have:
    /// gallery artifacts (nothing can be imported).
    fn bind_inputs(&self, request: &mut GenerationRequest) -> Result<(), RpcError> {
        for i in 0..request.inputs.len() {
            let artifact_id = match &request.inputs[i].source {
                GenerationInputSource::Asset { .. } => {
                    return Err(failed("studio asset was not found"));
                }
                GenerationInputSource::Artifact { artifact_id } => *artifact_id,
            };
            let (ti, ui, ri, ai) = self
                .locate_artifact(artifact_id)
                .ok_or_else(|| failed(ARTIFACT_NOT_FOUND))?;
            let artifact = &self.threads[ti].turns[ui].runs[ri].artifacts[ai];
            let input = &mut request.inputs[i];
            if input.content_hash.is_empty() {
                input.content_hash = artifact.content_hash.clone();
            } else if input.content_hash != artifact.content_hash {
                return Err(invalid(
                    "studio input content hash does not match the artifact",
                ));
            }
            if matches!(
                request.operation,
                MediaOperation::Upscale | MediaOperation::ImageEdit
            ) && input.role.as_str() == ROLE_SOURCE
                && let (Some(w), Some(h)) = (artifact.width, artifact.height)
            {
                request.display_aspect_ratio = (w, h);
            }
        }
        Ok(())
    }

    fn new_run(
        &mut self,
        position: u32,
        prepared: Prepared,
        at: DateTime<Utc>,
        recipe: (u32, u32),
    ) -> StudioRunView {
        let Prepared {
            model,
            request,
            quote,
        } = prepared;
        let id = StudioRunId::new();
        let view = StudioRunView {
            id,
            position,
            provider_id: request.provider_id.clone(),
            model,
            controls: request.controls.clone(),
            output_count: request.output_count,
            display_aspect_ratio: request.display_aspect_ratio,
            state: StudioRunState::Queued,
            progress: None,
            error: None,
            quote,
            prompt: (!request.prompt.is_empty()).then(|| request.prompt.clone()),
            inputs: request.inputs.clone(),
            artifacts: Vec::new(),
        };
        self.runs.insert(
            id,
            RunMeta {
                request,
                updated_at: at,
                style: recipe.0,
                palette: recipe.1,
            },
        );
        view
    }

    /// The store's `create_turn`: a new turn at the end, runs queued.
    fn create_turn(
        &mut self,
        conversation_id: StudioConversationId,
        prompt: &str,
        source_turn_id: Option<StudioTurnId>,
        prepared: Vec<Prepared>,
    ) -> Result<Vec<StudioRunId>, RpcError> {
        let trimmed = prompt.trim();
        if trimmed.chars().count() > 32_000 {
            return Err(invalid("prompt must contain 1 to 32000 characters"));
        }
        if prepared.is_empty() || prepared.len() > MAX_CREATE_TURN_RUNS {
            return Err(invalid("a turn must contain 1 to 16 model runs"));
        }
        if trimmed.is_empty()
            && !prepared
                .iter()
                .all(|run| run.request.operation == MediaOperation::Upscale)
        {
            return Err(invalid("prompt must contain 1 to 32000 characters"));
        }
        if !self
            .threads
            .iter()
            .any(|t| t.id == conversation_id && !t.archived)
        {
            return Err(failed(CONVERSATION_NOT_FOUND));
        }
        let at = now();
        let recipe = recipe_for(trimmed);
        let runs: Vec<StudioRunView> = prepared
            .into_iter()
            .enumerate()
            .map(|(i, p)| self.new_run(i as u32, p, at, recipe))
            .collect();
        let ids = runs.iter().map(|r| r.id).collect();
        let thread = self.thread_mut(conversation_id)?;
        let position = thread.turns.len() as u32;
        thread.turns.push(StudioTurnView {
            id: StudioTurnId::new(),
            position,
            prompt: trimmed.to_owned(),
            source_turn_id,
            batch_id: StudioBatchId::new(),
            runs,
            created_at: at,
        });
        if position == 0
            && (thread.title == UNTITLED_STUDIO_TITLE
                || thread.title == LEGACY_UNTITLED_STUDIO_TITLE)
            && let Some(title) = title_from_prompt(trimmed)
        {
            thread.title = title;
        }
        thread.updated_at = at;
        Ok(ids)
    }

    /// The store's `turn_extend_spec`: the turn's distinct original runs.
    fn extend_spec(
        &self,
        turn_id: StudioTurnId,
    ) -> Result<(String, Vec<StudioModelRunSpec>), RpcError> {
        let (thread, turn) = self
            .threads
            .iter()
            .find_map(|t| t.turns.iter().find(|u| u.id == turn_id).map(|u| (t, u)))
            .ok_or_else(|| failed(TURN_NOT_FOUND))?;
        if thread.archived {
            return Err(failed(CONVERSATION_NOT_FOUND));
        }
        let mut specs: Vec<StudioModelRunSpec> = Vec::new();
        for run in &turn.runs {
            let Some(meta) = self.runs.get(&run.id) else {
                continue;
            };
            let r = &meta.request;
            if specs.iter().any(|s| {
                s.provider_id == r.provider_id
                    && s.model_id == r.model_id
                    && s.controls == r.controls
                    && s.output_count == r.output_count
                    && s.display_aspect_ratio == r.display_aspect_ratio
            }) {
                continue;
            }
            specs.push(StudioModelRunSpec {
                provider_id: r.provider_id.clone(),
                model_id: r.model_id.clone(),
                operation: r.operation,
                output_count: r.output_count,
                controls: r.controls.clone(),
                inputs: r.inputs.clone(),
                manifest_version: r.manifest_version.clone(),
                display_aspect_ratio: r.display_aspect_ratio,
            });
        }
        if specs.is_empty() {
            return Err(invalid("turn has no model runs to generate more from"));
        }
        Ok((turn.prompt.clone(), specs))
    }

    /// The store's `extend_turn`: more runs on an existing turn.
    fn extend_turn(
        &mut self,
        turn_id: StudioTurnId,
        prepared: Vec<Prepared>,
    ) -> Result<(StudioConversationId, Vec<StudioRunId>), RpcError> {
        if prepared.is_empty() || prepared.len() > MAX_CREATE_TURN_RUNS {
            return Err(invalid("generate more must add 1 to 16 model runs"));
        }
        let (ti, ui) = self
            .threads
            .iter()
            .enumerate()
            .find_map(|(ti, t)| {
                t.turns
                    .iter()
                    .position(|u| u.id == turn_id)
                    .map(|ui| (ti, ui))
            })
            .ok_or_else(|| failed(TURN_NOT_FOUND))?;
        if self.threads[ti].archived {
            return Err(failed(CONVERSATION_NOT_FOUND));
        }
        let turn = &self.threads[ti].turns[ui];
        if turn.runs.len() + prepared.len() > MAX_TURN_RUNS {
            return Err(invalid(format!(
                "a turn can contain at most {MAX_TURN_RUNS} model runs"
            )));
        }
        let next = turn.runs.iter().map(|r| r.position + 1).max().unwrap_or(0);
        let recipe = recipe_for(&turn.prompt);
        let at = now();
        let runs: Vec<StudioRunView> = prepared
            .into_iter()
            .enumerate()
            .map(|(i, p)| self.new_run(next + i as u32, p, at, recipe))
            .collect();
        let ids = runs.iter().map(|r| r.id).collect();
        let thread = &mut self.threads[ti];
        thread.turns[ui].runs.extend(runs);
        thread.updated_at = at;
        Ok((thread.id, ids))
    }

    /// A finished run's artifacts, painted from its recipe.
    fn outputs(&mut self, meta: &RunMeta, at: DateTime<Utc>) -> Vec<StudioArtifactView> {
        let request = &meta.request;
        let aspect = aspect_of(&request.controls, request.display_aspect_ratio);
        let video = is_video(request.operation);
        let count = if video {
            1
        } else {
            request.output_count.max(1)
        };
        (0..count)
            .map(|position| {
                self.variation += 1;
                let seed = png::art_seed(meta.style, (meta.palette + position) % 8, self.variation);
                let art = if video {
                    let resolution = match request.controls.get(&ControlId::from("resolution")) {
                        Some(ControlValue::Resolution { value }) => value.as_str(),
                        _ => "720p",
                    };
                    let (width, height) = video_dimensions(aspect, resolution);
                    Art {
                        seed,
                        kind: MediaKind::Video,
                        width,
                        height,
                    }
                } else {
                    let (width, height) = fit(aspect, ORIGINAL_LONG_EDGE);
                    Art {
                        seed,
                        kind: MediaKind::Image,
                        width,
                        height,
                    }
                };
                let duration = match request.controls.get(&ControlId::from("duration")) {
                    Some(ControlValue::DurationSeconds { value }) => *value,
                    _ => 5.0,
                };
                self.artifact(art, position, at, video.then_some(duration))
            })
            .collect()
    }

    fn artifact(
        &mut self,
        art: Art,
        position: u32,
        at: DateTime<Utc>,
        duration: Option<f64>,
    ) -> StudioArtifactView {
        let id = StudioArtifactId::new();
        let (mime, size) = match art.kind {
            MediaKind::Image => ("image/png", png::encoded_len(art.width, art.height)),
            // ~1.1 MB/s at 720p, scaled by pixel count.
            MediaKind::Video => {
                let rate = 1_100_000.0 * (art.width * art.height) as f64 / (1280.0 * 720.0);
                ("video/mp4", (rate * duration.unwrap_or(5.0)) as u64)
            }
        };
        let recipe = format!(
            "zeron-demo-art:{}:{}x{}:{mime}",
            art.seed, art.width, art.height
        );
        self.art.insert(id, art);
        StudioArtifactView {
            id,
            output_position: position,
            media_kind: art.kind,
            mime_type: mime.to_owned(),
            size_bytes: size,
            width: Some(art.width),
            height: Some(art.height),
            duration_seconds: duration,
            metadata: Value::Null,
            created_at: at,
            thumbhash: Some(thumbhash_of(&art)),
            content_hash: format!("{:x}", Sha256::digest(recipe.as_bytes())),
        }
    }

    /// The opening dataset: [`THREADS`], laid out backwards from `now`.
    fn seed(&mut self, now: DateTime<Utc>) {
        let minutes = chrono::Duration::minutes;
        for spec in THREADS {
            let updated_at = now - minutes(spec.updated_ago_min);
            let count = spec.turns.len() as i64;
            let first = updated_at - minutes(spec.pace_min * (count - 1));
            let mut turns = Vec::new();
            let mut last_settled = first;
            for (index, seed_turn) in spec.turns.iter().enumerate() {
                let turn_at = first + minutes(spec.pace_min * index as i64);
                let mut runs = Vec::new();
                for (position, seed_run) in seed_turn.1.iter().enumerate() {
                    let SeedRun(model_id, aspect, outputs, outcome) = seed_run;
                    let Some(model) = self.catalog.iter().find(|m| m.id.as_str() == *model_id)
                    else {
                        continue;
                    };
                    let mut controls = BTreeMap::new();
                    controls.insert(
                        ControlId::from("aspect_ratio"),
                        ControlValue::AspectRatio {
                            width: aspect.0,
                            height: aspect.1,
                        },
                    );
                    if is_video(model.operation) {
                        controls.insert(
                            ControlId::from("duration"),
                            ControlValue::DurationSeconds { value: 5.0 },
                        );
                    }
                    let spec_run = StudioModelRunSpec {
                        provider_id: model.provider_id.clone(),
                        model_id: model.id.clone(),
                        operation: model.operation,
                        output_count: *outputs,
                        controls: with_defaults(model, controls),
                        inputs: Vec::new(),
                        manifest_version: model.manifest_version.clone(),
                        display_aspect_ratio: *aspect,
                    };
                    let Ok(mut prepared) = self.prepare(seed_turn.0, vec![spec_run], true) else {
                        continue;
                    };
                    let palette = spec.palettes[(index + position) % 2];
                    let mut view = self.new_run(
                        position as u32,
                        prepared.remove(0),
                        turn_at,
                        (spec.style, palette),
                    );
                    let settled = ms(turn_at + chrono::Duration::seconds(18 + 6 * position as i64));
                    match outcome {
                        Outcome::Done => {
                            let meta = self.runs[&view.id].clone();
                            view.artifacts = self.outputs(&meta, settled);
                            view.state = StudioRunState::Succeeded;
                        }
                        Outcome::Failed(message) => {
                            view.state = StudioRunState::Failed;
                            view.error = Some((*message).to_owned());
                        }
                        Outcome::Running(progress) => {
                            view.state = StudioRunState::Running;
                            view.progress = Some(*progress);
                        }
                    }
                    if !matches!(outcome, Outcome::Running(_)) {
                        last_settled = last_settled.max(settled);
                    }
                    if let Some(meta) = self.runs.get_mut(&view.id) {
                        meta.updated_at = if view.state.is_creating() {
                            turn_at
                        } else {
                            settled
                        };
                    }
                    runs.push(view);
                }
                turns.push(StudioTurnView {
                    id: StudioTurnId::new(),
                    position: index as u32,
                    prompt: seed_turn.0.to_owned(),
                    source_turn_id: None,
                    batch_id: StudioBatchId::new(),
                    runs,
                    created_at: ms(turn_at),
                });
            }
            let last_seen_at = if spec.unseen {
                // Looked in while the last turn was still generating.
                updated_at + chrono::Duration::seconds(2)
            } else {
                (last_settled + minutes(2)).min(now)
            };
            self.threads.push(Thread {
                id: StudioConversationId::new(),
                title: spec.title.to_owned(),
                created_at: ms(first - chrono::Duration::seconds(40)),
                updated_at: ms(updated_at),
                archived: spec.archived,
                forked_from_turn_id: None,
                last_seen_at: Some(ms(last_seen_at)),
                turns,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_normalizes_every_canned_model() {
        let models = catalog(Utc::now());
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, [GPT, SEEDREAM, QWEN, SD35, SEEDANCE]);
        assert!(models.iter().all(|m| m.is_picker_visible()));
        assert!(models.iter().all(|m| !m.manifest_version.is_empty()));
    }

    #[test]
    fn seed_covers_the_screens() {
        let studio = DemoStudio::new(CancellationToken::new(), true);
        let state = lock(&studio.state);
        assert_eq!(state.threads.len(), THREADS.len());
        let page = state.gallery_page(None, None);
        assert!(
            (75..=95).contains(&page.artifacts.len()),
            "{}",
            page.artifacts.len()
        );
        let runs: Vec<&StudioRunView> = state
            .threads
            .iter()
            .flat_map(|t| &t.turns)
            .flat_map(|t| &t.runs)
            .collect();
        assert!(runs.iter().any(|r| r.state == StudioRunState::Failed));
        assert!(runs.iter().any(|r| r.state.is_creating()));
        assert!(
            runs.iter()
                .all(|r| r.quote.is_some() || is_video(r.model.operation))
        );
        let summaries = state.conversations(true);
        assert!(summaries.iter().any(|s| s.done));
        assert!(summaries.iter().any(|s| s.creating));
        assert!(summaries.iter().any(|s| s.archived));
    }

    #[test]
    fn prompts_fail_on_the_word_only() {
        assert!(prompt_fails("please fail this"));
        assert!(prompt_fails("Fail."));
        assert!(!prompt_fails("failure is fine"));
    }

    #[test]
    fn video_dimensions_follow_resolution() {
        assert_eq!(video_dimensions((16, 9), "720p"), (1280, 720));
        assert_eq!(video_dimensions((9, 16), "1080p"), (1080, 1920));
        assert_eq!(fit((3, 2), 1024), (1024, 683));
    }
}
