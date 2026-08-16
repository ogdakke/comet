# Studio plan

## Goal

Add a first-class image and video generation studio to Zeron. The engine owns provider calls,
credentials, durable jobs, and generated artifacts; the GPUI app is a viewport over that state.
Venice AI is the first provider, but no Venice request or response type crosses the provider adapter
boundary.

The first useful release should let a user:

1. add and validate a Venice inference API key;
2. browse currently available image and video models;
3. select several image models, configure each model independently, and submit them as one turn;
4. iterate indefinitely inside a studio conversation without mutating its earlier prompts or results;
5. copy any past turn into the composer, tweak it, and generate a new related turn;
6. leave and reopen the studio without losing job progress;
7. inspect, retry, download, reveal, or delete generated artifacts;
8. later add video generation and masked image editing without replacing the conversation model.

## Fit with the current architecture

The feature belongs in the existing engine/RPC/UI split, but it should not be implemented as a new
agent harness. Harnesses describe interactive coding-agent runs and their transcript protocol;
media providers describe finite generation jobs, model capabilities, artifacts, and optional quote,
poll, and cancellation operations.

- `zeron-proto`: provider-neutral studio entities and RPC payloads.
- `zeron-engine`: provider registry, credentials, job runner, SQLite catalog, artifact store, and
  recovery.
- `zeron-rpc`: method names and existing unary/stream transport.
- `zeron-ui`: a global Studio route, generation form, job/gallery view, and Providers settings.
- `edge`: no dependency for the first release. Large media must not be put into Loro updates or the
  device relay.

Generated media is workspace-profile data, so store it below the active profile root, alongside the
profile's docs and uploads rather than in device-global settings. Provider credentials are
device-scoped secrets and are never synced. A job therefore records the device that owns and can
resume it.

Proposed layout:

```text
{profile_store_root}/studio/
  studio.sqlite3
  artifacts/{artifact_id}.{ext}
  previews/{artifact_id}.webp
  inputs/{asset_id}.{ext}

{device_root}/provider-accounts/
  provider metadata only; secrets use the platform secret store
```

## Provider contract

Create `crates/studio` (`zeron-studio`) for provider-neutral domain types and provider adapter
interfaces. Keeping adapters out of `zeron-engine` makes them unit-testable without assembling a
workspace engine and gives future providers one clear extension point.

```rust
#[async_trait]
pub trait MediaProvider: Send + Sync {
    fn id(&self) -> ProviderId;
    fn submission_capabilities(&self) -> SubmissionCapabilities;
    async fn validate_credentials(&self, secret: &Secret) -> Result<ProviderAccount>;
    async fn list_models(&self, secret: &Secret) -> Result<Vec<MediaModel>>;
    async fn quote(&self, secret: &Secret, request: &GenerationRequest)
        -> Result<Option<Quote>>;
    async fn submit(&self, secret: &Secret, request: &GenerationRequest, context: &SubmitContext)
        -> Result<Submission>;
    async fn reconcile(&self, secret: &Secret, attempt: &RemoteAttempt)
        -> Result<Option<Submission>>;
    async fn poll(&self, secret: &Secret, remote_job: &RemoteJob)
        -> Result<PollResult>;
    async fn cancel(&self, secret: &Secret, remote_job: &RemoteJob) -> Result<CancelResult>;
}
```

`submit` supports both provider shapes:

- `Submission::Completed(Vec<ProviderArtifact>)` for synchronous generation;
- `Submission::Queued(RemoteJob)` for asynchronous generation.

`poll` returns queued/running progress, completed downloadable artifact(s), or a normalized failure.
The engine, not the adapter, schedules polling with capped exponential backoff, persists every state
transition, downloads results atomically, and resumes non-terminal jobs after restart.

`SubmissionCapabilities` declares whether a provider accepts a client idempotency key and whether it
can reconcile an uncertain submission by that key or another durable request identifier. The engine
always supplies a stable local key, but must not imply guarantees the provider does not offer.

### Capability-driven models

Do not expose a lowest-common-denominator form or a bag of Venice fields. Each `MediaModel` carries:

- operation: text-to-image, image-to-image/edit, upscale, text-to-video, image-to-video,
  reference-to-video, or video-to-video;
- accepted input roles and count/size/MIME constraints;
- output media kind and possible formats;
- prompt and negative-prompt limits;
- a list of controls (`enum`, integer, number, boolean, dimensions, aspect ratio, resolution,
  duration) with defaults, bounds, choices, and conditional visibility;
- pricing metadata when advertised by the provider;
- raw provider metadata retained inside the adapter/cache for forward compatibility, never required
  by the UI.

One composer submission is a `GenerationBatch`: a shared prompt plus one or more `ModelRunSpec`
records. Every run selects its own provider, model, output count, inputs, and validated
`BTreeMap<ControlId, ControlValue>`. This permits, for example, two Venice models with four images
each and different aspect ratio, resolution, quality, seed, safety, or style settings. Runs execute
concurrently with a provider/account-specific limit; one failure does not discard successful sibling
runs.

The provider adapter translates semantic control IDs into its wire format. Unknown provider fields
cannot bypass validation. The exact submitted manifest version and settings are saved with every run
so an old generation remains explainable even after a model changes or disappears.

Model discovery and parameter discovery are related but separate:

1. **Live provider catalog:** ask the provider for current model IDs, media/operation type, pricing,
   capabilities, and model-specific constraints. For Venice this is `GET /api/v1/models` filtered by
   `image`, `inpaint`, `upscale`, or `video`.
2. **Versioned endpoint schema:** generate the adapter's known controls from the provider's official
   OpenAPI schema at development time. This supplies endpoint-level types, bounds, defaults, and
   conditional rules that a model list commonly omits.
3. **Small adapter overrides:** maintain reviewed model/family overrides only where the official live
   catalog and schema together are incomplete. Each override includes its source and review date.

Merge precedence is live model constraint, then reviewed model/family override, then endpoint
default. Never infer support from a similarly named model. If support cannot be established, hide the
control rather than sending it optimistically. If a request still receives an invalid-parameter or
removed-model response, refresh the catalog once and surface the provider error without silently
dropping a setting.

Cache the merged manifests with a fetched timestamp and keep the last successful catalog for offline
UI. A CI drift check compares the checked-in/generated Venice schema with its official OpenAPI spec;
the application does not download and interpret documentation at runtime.

## Durable domain model

Use SQLite for transactional job and artifact metadata. Loro is a poor fit for high-frequency job
progress and must never contain media bytes.

Core records:

- `ProviderConnection`: provider ID, display label, secret reference, validation state, timestamps;
- `StudioConversation`: user-created iteration boundary, title, timestamps, archived flag, and
  optional `forked_from_turn_id`;
- `StudioTurn`: immutable submitted prompt snapshot, conversation ID, position, optional
  `source_turn_id`, timestamps;
- `StudioBatch`: one turn's multi-model submission and aggregate state;
- `StudioRun`: batch ID, provider/model/operation, exact model-manifest/settings snapshot, owner
  device ID, aggregate status, quote snapshot, progress, error, timestamps;
- `StudioRunInput`: run ID, semantic role, ordinal, source kind plus exactly one asset/artifact ID,
  and the source's content hash at submission time;
- `StudioAttempt`: run ID, local idempotency key, canonical provider request snapshot (large media by
  hash/reference) and wire hash, remote job ID, state, credential/account reference,
  submit/retry/poll timestamps, bounded sanitized provider response metadata, and error;
- `StudioAsset`: immutable input copied into the profile store, MIME, size, hash, dimensions;
- `StudioArtifact`: run and successful attempt IDs, media kind, local path, MIME, size,
  dimensions/duration, seed and other reproducibility metadata, preview path, timestamps;
- `ModelCatalog`: provider ID, serialized normalized catalog, fetched/expiry timestamps.

Relatedness is explicit, not guessed:

- everything in one `StudioConversation` belongs to the same user-directed line of exploration;
- every submit appends a new immutable `StudioTurn`, even when its prompt text is identical;
- “Use prompt” copies a past turn's prompt, model selection, and settings into a draft, then records
  `source_turn_id` when submitted;
- media lineage comes only from `StudioRunInput`, not a singular turn field. A run may reference any
  number of assets/artifacts with ordered roles such as `source`, `mask`, `reference`, `control`,
  `first_frame`, `last_frame`, or `audio`;
- “New conversation from here” creates a separate conversation with `forked_from_turn_id`;
- a normalized prompt hash may support search or duplicate warnings, but never defines identity or
  grouping.

Statuses are `draft`, `quoting`, `awaiting_confirmation`, `queued`, `running`, `downloading`,
`succeeded`, `failed`, `cancelling`, and `cancelled`. A persisted transition table or event column
should retain enough history to diagnose retries without putting logs in the UI model.

Those are aggregate run/UI states. Attempts additionally distinguish `prepared`, `submitting`,
`submission_unknown`, `queued`, `running`, `succeeded`, `failed`, and `cancelled`. Retrying a run
creates a new immutable attempt; it never overwrites the prior remote job ID or request/response
history. Only one attempt may be actively submitting/running for a run unless the user explicitly
accepts the possibility of duplicate provider work.

Enforce a unique `(run_id, role, ordinal)` input slot and a database check that exactly one of
`asset_id` or `artifact_id` is populated. Re-verify the stored content hash before submit/resume so a
role can never silently point at changed bytes. Attempt credential references identify the provider
connection/account used for diagnosis; they never copy the secret into profile data.

Important invariants:

- write downloads to a same-directory temporary file, verify size/MIME, then atomically publish;
- keep the provider result until the local publish succeeds; for Venice video, call its completion
  cleanup only after a durable local file exists;
- write the attempt and local idempotency key before the first network byte is sent;
- automatically retry a submit only when the provider contract makes it safe through idempotency, or
  after reconciliation proves no remote job exists;
- when a non-idempotent/non-reconcilable submit times out ambiguously, persist
  `submission_unknown`. Do not auto-resubmit. Offer Check status when possible, Retry anyway with an
  explicit duplicate-work/cost warning, and Mark failed;
- polling retries are safe only after a durable remote job ID exists and never create a new attempt;
- retries create a new attempt attached to the same run and preserve every earlier attempt;
- editing a past prompt never changes its original turn; drafts are mutable, submitted turns are not;
- remove a credential only after warning about jobs that still need it;
- artifact deletion is explicit and jailed to the profile's studio directories.

## Venice adapter

Use Venice's native endpoints instead of its reduced OpenAI-compatible image endpoint.

- Models: `GET /api/v1/models?type=image`, `?type=inpaint`, `?type=upscale`, and `?type=video`,
  normalized and merged with the adapter's schema as described above.
- Image: `POST /api/v1/image/generate`; this is synchronous and returns base64 or binary. Prefer
  binary for a single output and JSON for variants, with response limits enforced before decode.
- Image styles: `GET /api/v1/image/styles` when a selected model exposes style presets.
- Future image edit: `POST /api/v1/image/edit` for prompt edits and `/api/v1/image/multi-edit` for
  a base image plus a full-resolution mask/overlay. The canvas stores the user's brush strokes as a
  non-destructive mask asset; it never paints over the source artifact.
- Video quote: `POST /api/v1/video/quote`.
- Video submit: `POST /api/v1/video/queue`, persisting both model ID and `queue_id` before polling.
- Video poll/download: `POST /api/v1/video/retrieve`; distinguish JSON processing responses from
  completed `video/mp4` by content type.
- Video cleanup: `POST /api/v1/video/complete` only after the local artifact is safely published.
- Account health: use a cheap authenticated models request for validation; balance/rate-limit data
  can be a later optional provider capability.

The adapter needs explicit handling for 401 (credential invalid), 402 (insufficient balance), 429
(retry after provider guidance), transient 5xx/network errors, invalid content types, maximum
response sizes, and model removal between catalog load and submission.

Venice documentation used for this plan:

- https://docs.venice.ai/api-reference/endpoint/image/generate
- https://docs.venice.ai/api-reference/endpoint/video/quote
- https://docs.venice.ai/api-reference/endpoint/video/queue
- https://docs.venice.ai/api-reference/endpoint/video/retrieve
- https://docs.venice.ai/api-reference/endpoint/video/complete
- https://docs.venice.ai/api-reference/endpoint/models/list
- https://docs.venice.ai/swagger.yaml
- https://docs.venice.ai/api-reference/endpoint/image/edit
- https://docs.venice.ai/api-reference/endpoint/image/multi-edit

## RPC surface

Add typed payloads to `zeron-proto` and method constants/dispatch to the current RPC boundary:

- `ListStudioProviders`
- `SetStudioProviderCredential`
- `RemoveStudioProviderCredential`
- `ValidateStudioProvider`
- `ListStudioModels { providerId, mediaKind, refresh }`
- `ListStudioConversations`
- `CreateStudioConversation`
- `RenameStudioConversation`
- `ArchiveStudioConversation`
- `WatchStudioConversation { conversationId }`
- `ImportStudioAsset` (chunked, with higher configurable limits than chat's 32 MiB image upload)
- `QuoteStudioBatch`
- `CreateStudioTurn { conversationId, prompt, runs, sourceTurnId? }`, where each run carries an
  ordered list of typed inputs
- `ConfirmStudioBatch`
- `CancelStudioRun`
- `RetryStudioRun`
- `DeleteStudioArtifact`
- `WatchStudioJobs` (initial snapshot followed by changes)
- `ReadStudioArtifactChunk` or a short-lived local media URL for previews/playback

Studio calls default to the directly attached engine. In the first release they are IPC-local and
not device-relay-forwardable. This avoids moving large inputs/results through the relay and makes job
ownership obvious. Remote execution can later forward only commands and use direct blob transfer or
object storage for media.

Never return an unrestricted filesystem path as authority. Artifact reads take an artifact ID and
the engine resolves it inside the profile store. Export is a separate user-selected destination.

## UI

Add `Route::Studio`, reachable from a persistent sidebar icon/entry, and a `Providers` settings
section. Studio is global to the active profile rather than nested under a coding session.

The primary UI is a conversation feed, not a flat global generation gallery. A separate conversation
index provides New conversation, recent conversations, rename, archive, and search. Inside a
conversation:

- the feed is chronological and each turn renders a compact prompt header followed by its output
  grid; pending tiles occupy their final positions immediately;
- “Use prompt” loads the turn's exact prompt and model-run settings into the bottom composer without
  changing history; submitting appends at the end of the current conversation;
- “Generate again” resubmits the same snapshot as a new turn, including fresh random seeds unless
  the user explicitly locks them;
- an artifact inspector offers full-size preview, metadata, download/reveal/delete, use as an image
  input, and later Edit with mask;
- the sticky bottom composer contains the shared prompt and a horizontal list of selected model
  cards. Each card has output count and a settings popover generated from that model's manifest.
  Adding/removing/reordering models affects only the draft.

### Feed grid contract

The grid is a first-class layout primitive with these initial constants:

- centered content width: `min(available_width - 2 * gutter, 1600px)`;
- maximum four equal-width columns;
- responsive columns: 1 below 520px, 2 from 520px, 3 from 900px, and 4 from 1240px, evaluated
  against the feed content width rather than the whole window;
- 12px gap on compact widths and 16px otherwise;
- every turn starts on a new grid row and its artifacts remain in stable model/run/output order;
- tiles use the run's requested aspect ratio. Mixed ratios render uncropped with a neutral letterbox
  inside the equal-width cell; clicking opens the uncropped full-size asset;
- incomplete final rows stay left-aligned rather than stretching two images to four-image width;
- virtualize by turn and retain measured turn heights, reusing the transcript's existing anchor and
  image-cache techniques so long conversations do not reflow while scrolling.

This intentionally differs from the supplied reference where the floating composer obscures the
bottom of the gallery: the Zeron composer reserves a bottom safe area, while older generations can
scroll behind a subtle fade. The feed remains readable and the composer remains continuously
available.

### Aspect-ratio loading skeletons

Every `ModelRunSpec` resolves a `display_aspect_ratio` before submission. Resolution order is the
run's explicit aspect ratio, explicit width/height, selected model default, then `1:1` only as a last
fallback. Persist that value on the run and allocate one skeleton tile per requested output as soon
as the turn is appended. The skeleton uses the exact final grid width, aspect ratio, corner radius,
and ordering, so queued, running, downloading, decoded, and failed states never cause feed reflow.

Skeleton states remain visually quiet:

- queued: low-contrast neutral fill;
- running: slow diagonal light/noise movement with a per-tile seed and stagger;
- downloading/decoding: the same tile with a restrained progress wash when real progress exists;
- failed: static surface with the run error/retry affordance;
- succeeded: overlap the decoded image and skeleton, then reveal the image without changing layout.

The currently pinned GPUI fork does not expose a public API for attaching an arbitrary fragment
shader to a normal element. Its `runtime_shaders` feature recompiles GPUI's built-in Metal shader
library during development; it is not an application shader plug-in system. GPUI's public scene is a
fixed set of quads, paths, sprites/images, and surfaces.

The initial implementation uses ordinary GPUI painting only: a clipped neutral tile with a soft
diagonal gradient translating across it, followed by a 220ms opacity crossfade once the real image is
fully decoded. The shimmer is paint-only, never changes layout, animates only while its tile is
visible and pending, and stops requesting frames immediately on completion. Reduced motion renders a
static neutral tile and an immediate image or very short crossfade.

A custom `EffectQuad` / `EffectSprite` renderer primitive and shader-based image dissolve are optional
future polish, not an MVP dependency. If pursued later, implement a small fixed set of audited effects
with equivalent Metal and WGSL behavior rather than compiling arbitrary user/provider shader source
at runtime.

### Image lightbox route

Clicking a generated image opens a routed, immersive artifact page rather than a transient dialog:
`Route::StudioArtifact { conversation_id, artifact_id }`. Back/close restores the conversation's
exact scroll anchor and focused tile. The route makes the viewer feel like a real workspace, while
its stage keeps the classic near-black lightbox background.

The navigation sequence is every successfully generated image artifact in the conversation, sorted
by turn position, model-run position, and output position. Prompt equality and current feed
virtualization do not affect the sequence. Input/reference assets are excluded unless they also exist
as generated artifacts in that conversation.

Layout:

- a flexible black image stage occupies the available center;
- a persistent 320px right inspector contains the image's prompt, model/settings metadata and
  actions; Download is the first-release action, with Use as input, Edit, and Make video added only
  when those workflows ship;
- a bottom, horizontally scrollable filmstrip contains small thumbnails for the entire conversation;
  the selected thumbnail is slightly larger and outlined, selection is kept centered when practical,
  and thumbnail decoding/loading is virtualized;
- previous/next hit targets remain available at the stage edges without covering the image;
- on narrow windows the inspector becomes a toggleable drawer and the filmstrip remains available.

Viewer transform state is `(fit_scale, user_zoom, pan_x, pan_y)`. “Fully out” means fit-to-stage,
not 100% source pixels. Start every newly selected image at fit-to-stage and clamp pan so the image
cannot be permanently lost offscreen.

Pointer and gesture contract:

- mouse wheel over the stage zooms around the pointer location in small steps;
- trackpad pinch zooms continuously around the gesture centroid;
- double-click toggles between fit-to-stage and `2x` fit scale around the clicked point;
- click-drag on the image pans it when zoomed; the cursor changes from grab to grabbing;
- horizontal trackpad scroll changes images while at fit scale;
- while zoomed, horizontal scroll pans first. Only a continued outward swipe at a horizontal pan
  boundary, past a distance/velocity threshold, changes images; pinch gestures never navigate;
- switching images resets zoom/pan to fit and uses a short reduced-motion-aware slide/crossfade;
- clicking the bare black stage outside the rendered image closes the route. Clicks in the image,
  inspector, filmstrip, arrows, or controls never bubble into close.

Keyboard contract:

- `Escape`: close and restore the originating feed position;
- `Left` / `Right`: previous/next image, including wrap-around;
- `Home` / `End`: first/last image;
- `+` / `-`: zoom in/out around stage center;
- `0`: fit-to-stage;
- `Space` plus pointer movement: temporary pan, matching creative tools;
- `Tab`: traverse close, navigation, filmstrip, and inspector actions with visible focus rings;
- focused text/controls consume their normal keys before the lightbox shortcuts.

Preload the full image for the current artifact and thumbnails plus full-size metadata for the two
neighbors on each side. Decode off the render path, reuse the studio image cache, cancel stale loads
after rapid navigation, and keep only a bounded number of full-resolution decodes resident.

Reuse the current attachment decoding/image cache patterns for image previews, but give studio media
its own size limits and cache eviction. Video generation can ship before portable embedded playback:
show a generated poster/metadata and provide Open/Reveal/Export using the OS player. Embedded video
is a separate platform spike because GPUI currently has no project-level video view; macOS
AVFoundation and the Linux packaging/runtime choice need evaluation before promising an in-canvas
player.

The iOS client is out of the first implementation slice. The RPC/domain design must remain usable by
it later, but adding native playback, background polling, and artifact transfer to iOS should be its
own milestone.

## Delivery phases

### Phase 0 — two short spikes

1. Capture real Venice model responses for representative image, text-to-video, and
   image-to-video models; prove the normalized control schema can render them without hardcoded model
   names.
2. Prove image rendering from an artifact ID and choose the first-release video experience
   (OS-player handoff versus a supported embedded playback path).

Exit: fixtures are checked in with secrets/media removed, capability gaps are documented, and no
unknown video playback dependency blocks the MVP.

### Phase 1 — conversation-based image slice

1. Add `zeron-studio` domain/provider interfaces and a fake provider.
2. Add secret storage, SQLite migrations, artifact jail, and recovery.
3. Add provider/model/job RPCs and streaming job state.
4. Implement Venice model/schema discovery and synchronous image generation.
5. Build the conversation index, four-column feed, multi-model composer, per-model settings, and
   artifact inspector.
6. Add exact-aspect skeleton allocation, a simple GPUI shimmer, and the decoded-image crossfade.
7. Build the routed image lightbox with conversation-wide keyboard/gesture navigation, zoom/pan,
   filmstrip, inspector, and Download.
8. Support use-prompt, generate-again, retry, fork-to-new-conversation, export/delete, and restart
   persistence.

Exit: a Venice text-to-image generation completes end to end, survives restart, and a second fake
provider is registered in tests without changing engine or UI run logic.

### Phase 2 — video jobs

1. Add studio asset import with validated image references.
2. Implement Venice quote, queue, retrieve, download, and cleanup.
3. Add durable poll scheduling, backoff, cancellation semantics, and restart resumption.
4. Add text-to-video and image-to-video UI, quote confirmation, progress, poster, and playback/handoff.

Exit: closing and reopening Zeron during generation resumes the job exactly once and preserves the
completed MP4 locally.

### Phase 3 — non-destructive image editing

1. Add a zoomable image editor with pan, brush size/hardness, erase, undo/redo, and mask overlay.
2. Store the mask at source-image resolution plus a small editable stroke document; never modify the
   source artifact.
3. Add edit-model discovery and Venice single/multi-edit submission.
4. Append each edit as a new turn linked to the source artifact, so edited outputs participate in the
   same feed and can themselves be edited indefinitely.

### Phase 4 — hardening and provider SDK

1. Document the adapter contract and add a second small provider adapter or conformance fixture.
2. Add quota/balance status as optional capabilities, telemetry that contains no prompts or media,
   and structured diagnostic export.
3. Add retention controls, cache cleanup, disk-space preflight, accessibility, reduced-motion, and
   large-gallery virtualization.
4. Optionally evaluate a cross-renderer `EffectQuad` / `EffectSprite` primitive for shader-based
   loading and image transitions after profiling the ordinary GPUI implementation.
5. Design opt-in metadata sync and remote artifact transport separately; do not overload registry
   rows or attachment relay RPCs.

## Tests and verification

- Contract tests run every provider against shared catalog, submit, poll, error, and cancellation
  expectations.
- Venice tests use sanitized HTTP fixtures for sync image, queued video, processing, completion,
  rate limit, insufficient funds, malformed payload, and removed model cases; live tests are ignored
  and require an explicit environment key.
- Engine tests cover crash points before/after provider submit and before/after artifact publish,
  restart resumption, idempotent retry, reconciliation, ambiguous non-idempotent submit, explicit
  retry-anyway, attempt history, multi-input role/ordering/hash integrity, secret removal, path
  traversal, oversized input/output, and profile isolation.
- RPC tests cover serialization compatibility, stream cancellation, reconnect snapshots, and the
  fact that secrets never appear in responses or logs.
- UI reducer/render tests cover dynamic per-model controls, multi-model partial failures, stale
  catalogs, quotes, all run states, conversation lineage, prompt reuse, stable four-column ordering,
  responsive breakpoints, feed cache eviction, lightbox ordering/wrap-around, route restoration,
  keyboard focus/shortcuts, zoom-at-pointer math, pan clamping, gesture arbitration, click-outside
  dismissal, thumbnail virtualization, bounded full-image caching, exact placeholder aspect/ordering,
  no-reflow state transitions, reduced-motion behavior, offscreen animation suspension, and shimmer
  lifecycle cleanup.
- End-to-end smoke: configure fake provider, generate an image, restart, inspect/export/delete; queue
  fake video, restart mid-poll, complete exactly once.

## Product decisions to confirm before Phase 1

Recommended defaults are:

1. **Scope:** studio history is profile-scoped and local to its owning device initially; no cloud
   gallery in v1.
2. **Video playback:** OS-player handoff in v1, embedded playback after a platform spike.
3. **Safety controls:** expose provider safety/watermark controls exactly when supported and persist
   their chosen values in the job; do not silently force adapter-specific values.
4. **Costs:** require explicit confirmation whenever the provider returns a quote; show “price not
   available” rather than estimating locally when it does not.
5. **Retention:** keep successful artifacts until explicit deletion; automatically prune abandoned
   input staging and failed partial downloads.

These choices give the smallest coherent slice while keeping the storage and provider contracts
ready for a synced, multi-device studio later.
