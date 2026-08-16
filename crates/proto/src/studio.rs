//! IPC-local Studio payloads. Credential-bearing requests never appear in responses.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeron_studio::{
    ControlId, ControlValue, GenerationInput, MediaKind, MediaModel, MediaOperation, ModelId,
    ProviderId, StudioConversationId, StudioTurnId,
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
