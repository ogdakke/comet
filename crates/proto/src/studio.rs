//! IPC-local Studio payloads. Credential-bearing requests never appear in responses.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeron_studio::{
    ControlId, ControlValue, GenerationInput, MediaKind, MediaModel, MediaOperation, ModelId,
    ProviderId, Quote, StudioArtifactId, StudioBatchId, StudioConversationId, StudioRunId,
    StudioTurnId,
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
    pub runs: Vec<StudioModelRunSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_turn_id: Option<StudioTurnId>,
}

/// Append another copy of a turn's original model runs under the same prompt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtendStudioTurnRequest {
    pub turn_id: StudioTurnId,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuoteStudioBatchRequest {
    pub prompt: String,
    pub runs: Vec<StudioModelRunSpec>,
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
