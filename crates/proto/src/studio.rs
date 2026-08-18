//! IPC-local Studio payloads. Credential-bearing requests never appear in responses.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeron_studio::{
    AccountBalance, ControlId, ControlValue, GenerationInput, MediaKind, MediaModel,
    MediaOperation, ModelId, ProviderId, Quote, StudioArtifactId, StudioAssetId, StudioBatchId,
    StudioConversationId, StudioRunId, StudioTurnId,
};

pub use zeron_studio::{
    AttachmentOrigin, AttachmentTrayView, BudgetKind, ChipView, ComposerAttachment,
    ComposerConflict, ComposerMediaKind, ComposerMode, ComposerPhase, ComposerSnapshot,
    ComposerView, ConflictCode, ConflictId, ConflictSeverity, ConflictSubjects, GlobalControls,
    LimitBudget, LimitHint, ResolveAction, ResolveActionView, STUDIO_VALIDATION_CODE,
    SelectedModelRef, SendState, StudioValidationError, TrayAccept,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ProviderValidationState {
    NotValidated,
    Valid,
    Invalid,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioProviderConnection {
    pub provider_id: ProviderId,
    pub display_label: String,
    pub configured: bool,
    pub validation_state: ProviderValidationState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation_message: Option<String>,
    /// Venice Safe Venice: when true, adult-classified images are returned blurred.
    /// Defaults to off so generations receive the original output.
    #[serde(default)]
    pub safe_mode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStudioProvidersResponse {
    pub providers: Vec<StudioProviderConnection>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetStudioProviderCredentialRequest {
    pub provider_id: ProviderId,
    pub display_label: String,
    pub secret: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioProviderRequest {
    pub provider_id: ProviderId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetStudioProviderPreferencesRequest {
    pub provider_id: ProviderId,
    pub safe_mode: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStudioModelsRequest {
    pub provider_id: ProviderId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_kind: Option<MediaKind>,
    #[serde(default)]
    pub refresh: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStudioModelsResponse {
    pub models: Vec<MediaModel>,
    pub fetched_at: DateTime<Utc>,
    pub stale: bool,
}

/// Default title for a newly created studio conversation. The first prompt
/// replaces this with a truncated copy of itself.
pub const UNTITLED_STUDIO_TITLE: &str = "Untitled thread";
/// Pre-rename default. First-prompt retitle still matches these older rows.
pub const LEGACY_UNTITLED_STUDIO_TITLE: &str = "Untitled study";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioConversationSummary {
    pub id: StudioConversationId,
    pub title: String,
    pub turn_count: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_turn_id: Option<StudioTurnId>,
    /// True while at least one run is still queued, generating, or downloading.
    #[serde(default)]
    pub creating: bool,
    /// Finished a run the user has not opened since — the green "Done" badge.
    #[serde(default)]
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStudioConversationsRequest {
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStudioConversationsResponse {
    pub conversations: Vec<StudioConversationSummary>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateStudioConversationRequest {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from_turn_id: Option<StudioTurnId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RenameStudioConversationRequest {
    pub conversation_id: StudioConversationId,
    pub title: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArchiveStudioConversationRequest {
    pub conversation_id: StudioConversationId,
    pub archived: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteStudioConversationRequest {
    pub conversation_id: StudioConversationId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkStudioConversationSeenRequest {
    pub conversation_id: StudioConversationId,
}

/// One independently configured model card in the composer.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioModelRunSpec {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub operation: MediaOperation,
    pub output_count: u32,
    pub controls: BTreeMap<ControlId, ControlValue>,
    pub inputs: Vec<GenerationInput>,
    pub manifest_version: String,
    pub display_aspect_ratio: (u32, u32),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateStudioTurnRequest {
    pub conversation_id: StudioConversationId,
    pub prompt: String,
    #[serde(default)]
    pub runs: Vec<StudioModelRunSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_turn_id: Option<StudioTurnId>,
    /// Live composer snapshot. When present, the engine re-evaluates and
    /// projects runs; client `runs` are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composer: Option<ComposerSnapshot>,
}

/// Append another copy of a turn's original model runs under the same prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendStudioTurnRequest {
    pub turn_id: StudioTurnId,
}

/// Append an edit or upscale run to the source artifact's existing turn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppendStudioDerivedRunRequest {
    pub source_artifact_id: StudioArtifactId,
    pub prompt: String,
    pub run: StudioModelRunSpec,
    /// Optional source-resolution mask PNG (white = edit region). Published as
    /// a studio asset and attached as `role = mask`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_png_base64: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteStudioBatchRequest {
    pub prompt: String,
    #[serde(default)]
    pub runs: Vec<StudioModelRunSpec>,
    /// Live composer snapshot. When present, the engine re-evaluates and
    /// projects runs; client `runs` are ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composer: Option<ComposerSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateStudioComposerRequest {
    pub composer: ComposerSnapshot,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<ProviderId>,
}

/// Chunked import. Handler is PR 4 — this crate only defines the frames.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStudioAssetRequest {
    pub asset_id: StudioAssetId,
    pub offset: u64,
    /// Standard base64 of this chunk's bytes.
    pub data: String,
    pub last: bool,
    /// SHA-256 hex. Required when `last` is true (enforced by the handler).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_hash: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime_hint: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportStudioAssetChunk {
    pub asset_id: StudioAssetId,
    pub next_offset: u64,
}

/// Not-last: `{ assetId, nextOffset }`. Last: a committed [`ComposerAttachment`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ImportStudioAssetResponse {
    Continue(ImportStudioAssetChunk),
    Complete(ComposerAttachment),
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteStudioRunView {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<Quote>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteStudioBatchResponse {
    pub runs: Vec<QuoteStudioRunView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total: Option<Quote>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioProviderBalanceResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<AccountBalance>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StudioRunState {
    Draft,
    Quoting,
    AwaitingConfirmation,
    Queued,
    Running,
    Downloading,
    Succeeded,
    Failed,
    Cancelling,
    Cancelled,
}

impl StudioRunState {
    /// Any non-terminal generation — the sidebar "Creating" label.
    pub fn is_creating(self) -> bool {
        !matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioArtifactView {
    pub id: StudioArtifactId,
    pub output_position: u32,
    pub media_kind: MediaKind,
    pub mime_type: String,
    pub size_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub metadata: serde_json::Value,
    pub created_at: DateTime<Utc>,
    /// Compact placeholder (standard base64 of a ThumbHash). Present once a
    /// preview has been derived; tiles paint this if the JPEG is not in RAM.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbhash: Option<String>,
    /// SHA-256 of the artifact bytes. Clients send this back when using the
    /// image as a generation input.
    #[serde(default)]
    pub content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioRunView {
    pub id: StudioRunId,
    pub position: u32,
    pub provider_id: ProviderId,
    pub model: MediaModel,
    pub controls: BTreeMap<ControlId, ControlValue>,
    pub output_count: u32,
    pub display_aspect_ratio: (u32, u32),
    pub state: StudioRunState,
    pub progress: Option<f32>,
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<Quote>,
    /// Prompt stored on the run. Present for image edits; generate runs usually
    /// match the turn prompt, and upscales are empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default)]
    pub inputs: Vec<GenerationInput>,
    pub artifacts: Vec<StudioArtifactView>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioTurnView {
    pub id: StudioTurnId,
    pub position: u32,
    pub prompt: String,
    pub source_turn_id: Option<StudioTurnId>,
    pub batch_id: StudioBatchId,
    pub runs: Vec<StudioRunView>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioConversationView {
    pub conversation: StudioConversationSummary,
    pub turns: Vec<StudioTurnView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WatchStudioConversationRequest {
    pub conversation_id: StudioConversationId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioGalleryItem {
    pub id: StudioArtifactId,
    pub conversation_id: StudioConversationId,
    pub turn_id: StudioTurnId,
    pub output_position: u32,
    pub media_kind: MediaKind,
    pub mime_type: String,
    pub size_bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub prompt: String,
    pub model_display_name: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thumbhash: Option<String>,
    /// Parent image for an edit or upscale artifact.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "upscaledFromArtifactId"
    )]
    pub source_artifact_id: Option<StudioArtifactId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListStudioArtifactsResponse {
    pub artifacts: Vec<StudioGalleryItem>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeleteStudioArtifactRequest {
    pub artifact_id: StudioArtifactId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RetryStudioRunRequest {
    pub run_id: StudioRunId,
    #[serde(default)]
    pub retry_anyway: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadStudioArtifactChunkRequest {
    pub artifact_id: zeron_studio::StudioArtifactId,
    #[serde(default)]
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioArtifactChunk {
    pub artifact_id: zeron_studio::StudioArtifactId,
    pub file_name: String,
    pub mime_type: String,
    /// Base64 of this chunk's byte range.
    pub data: String,
    pub next_offset: u64,
    pub done: bool,
}
