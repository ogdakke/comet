//! Pure studio composer: snapshot, tray mapping, evaluate, and resolve.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AudioCapability, CapabilityIntersection, ControlChoice, ControlId, ControlValue,
    GenerationInput, GenerationInputSource, InputConstraint, InputRole, MediaKind, MediaModel,
    MediaOperation, MimeConstraint, ModelControl, ModelId, ProviderId, ROLE_AUDIO, ROLE_ELEMENT,
    ROLE_KEYFRAME, ROLE_LAST_FRAME, ROLE_REFERENCE, ROLE_REFERENCE_AUDIO, ROLE_REFERENCE_VIDEO,
    ROLE_SCENE, ROLE_SOURCE, StudioArtifactId, StudioAssetId, StudioConversationId, StudioTurnId,
    intersect_video_globals, picker_models,
};

pub const QUEUE_BODY_LIMIT_BYTES: u64 = 35_000_000;

const DURATION_CONTROL: &str = "duration";
const ASPECT_CONTROL: &str = "aspect_ratio";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerSnapshot {
    pub conversation_id: Option<StudioConversationId>,
    pub mode: ComposerMode,
    pub prompt: String,
    pub duration: Option<ControlValue>,
    pub attachments: Vec<ComposerAttachment>,
    pub selected: Vec<SelectedModelRef>,
    pub source_turn_id: Option<StudioTurnId>,
    pub catalog_fetched_at: Option<DateTime<Utc>>,
}

impl Default for ComposerSnapshot {
    fn default() -> Self {
        Self {
            conversation_id: None,
            mode: ComposerMode::Image,
            prompt: String::new(),
            duration: None,
            attachments: Vec::new(),
            selected: Vec::new(),
            source_turn_id: None,
            catalog_fetched_at: None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerMode {
    Image,
    Video,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectedModelRef {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub output_count: u32,
    pub controls: BTreeMap<ControlId, ControlValue>,
}

impl SelectedModelRef {
    pub fn new(provider_id: impl Into<ProviderId>, model_id: impl Into<ModelId>) -> Self {
        Self {
            provider_id: provider_id.into(),
            model_id: model_id.into(),
            output_count: 1,
            controls: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerMediaKind {
    Image,
    Video,
    Audio,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerAttachment {
    pub id: StudioAssetId,
    pub kind: ComposerMediaKind,
    pub pending: bool,
    pub origin: AttachmentOrigin,
    pub mime_type: String,
    pub byte_size: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub duration_seconds: Option<f64>,
    pub content_hash: String,
    pub role_hint: Option<InputRole>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AttachmentOrigin {
    Asset,
    Artifact { artifact_id: StudioArtifactId },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerView {
    pub phase: ComposerPhase,
    pub mode: ComposerMode,
    pub send: SendState,
    pub globals: GlobalControls,
    pub models: Vec<ChipView>,
    pub attachments: AttachmentTrayView,
    pub budgets: Vec<LimitBudget>,
    pub hints: Vec<LimitHint>,
    pub conflicts: Vec<ComposerConflict>,
    pub catalog_stale: bool,
    pub open_picker: bool,
    pub refresh_catalog: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComposerPhase {
    Idle,
    Editing,
    NeedsResolution,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendState {
    pub enabled: bool,
    pub blocked_reason: Option<ConflictCode>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalControls {
    pub duration: Option<ControlValue>,
    pub duration_choices: Vec<ControlChoice>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChipView {
    pub model_id: ModelId,
    pub display_name: String,
    pub operation: MediaOperation,
    pub output_count: u32,
    pub controls: Vec<ModelControl>,
    pub values: BTreeMap<ControlId, ControlValue>,
    pub mapped_inputs: Vec<GenerationInput>,
    pub badge: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentTrayView {
    pub items: Vec<ComposerAttachment>,
    pub accept: TrayAccept,
    /// + button: an `(r|v|i)2(v|i)` model is selected. Paste/drop still work
    /// when this is false.
    pub add_enabled: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrayAccept {
    pub mime_types: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitBudget {
    pub kind: BudgetKind,
    pub used: u32,
    pub maximum: Option<u32>,
    pub subjects: Vec<ModelId>,
    pub remaining: Option<i32>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    PromptChars,
    Role { role: InputRole },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LimitHint {
    pub text: String,
    pub subjects: Vec<ModelId>,
}

/// UI / engine page state. Not persisted in the snapshot. Not returned by evaluate.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SendSession {
    #[default]
    Idle,
    Quoting,
    Sending,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ComposerEvent {
    SetMode {
        mode: ComposerMode,
        restore: Vec<SelectedModelRef>,
    },
    SetPrompt {
        text: String,
    },
    SetDuration {
        value: ControlValue,
    },
    Attach {
        attachment: ComposerAttachment,
    },
    Detach {
        asset_id: StudioAssetId,
    },
    PinRole {
        asset_id: StudioAssetId,
        role: InputRole,
    },
    SelectModel {
        provider_id: ProviderId,
        model_id: ModelId,
    },
    DeselectModel {
        model_id: ModelId,
    },
    ReplaceModels {
        selected: Vec<SelectedModelRef>,
    },
    SetModelControl {
        model_id: ModelId,
        control_id: ControlId,
        value: ControlValue,
    },
    SetOutputCount {
        model_id: ModelId,
        output_count: u32,
    },
    RestoreDraft {
        snapshot: ComposerSnapshot,
    },
    CatalogUpdated {
        fetched_at: DateTime<Utc>,
    },
    Resolve {
        conflict_id: ConflictId,
        action: ResolveAction,
    },
    Send,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictTrigger {
    SetMode,
    SelectModel,
    DeselectModel,
    ReplaceModels,
    CatalogUpdated,
    RestoreDraft,
    Attach,
    Detach,
    PinRole,
    Resolve,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ConflictId(pub String);

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ComposerConflict {
    pub id: ConflictId,
    pub code: ConflictCode,
    pub severity: ConflictSeverity,
    pub title: String,
    pub explanation: String,
    pub subjects: ConflictSubjects,
    pub actions: Vec<ResolveActionView>,
}

impl ComposerConflict {
    pub fn blocks_send(&self) -> bool {
        self.severity == ConflictSeverity::BlockSend
    }
}

/// Structured send/quote rejection. Proto re-exports this; do not duplicate.
pub const STUDIO_VALIDATION_CODE: &str = "studio_validation";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StudioValidationError {
    #[serde(default = "studio_validation_code")]
    pub code: String,
    pub conflicts: Vec<ComposerConflict>,
}

impl StudioValidationError {
    pub fn new(conflicts: Vec<ComposerConflict>) -> Self {
        Self {
            code: STUDIO_VALIDATION_CODE.to_owned(),
            conflicts,
        }
    }
}

fn studio_validation_code() -> String {
    STUDIO_VALIDATION_CODE.to_owned()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictSeverity {
    BlockSend,
    Warn,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConflictSubjects {
    pub model_ids: Vec<ModelId>,
    pub asset_ids: Vec<StudioAssetId>,
    pub control_ids: Vec<ControlId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveActionView {
    pub action: ResolveAction,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResolveAction {
    RemoveUnsupportedReferences {
        asset_ids: Vec<StudioAssetId>,
    },
    RemoveAllAttachments,
    DeselectIncompatibleModels {
        model_ids: Vec<ModelId>,
    },
    KeepModelsDropOthers {
        model_ids: Vec<ModelId>,
    },
    ClampDuration {
        value: ControlValue,
    },
    ClearDuration,
    RevertMode {
        mode: ComposerMode,
        selected: Vec<SelectedModelRef>,
        duration: Option<ControlValue>,
    },
    RevertModelSelection {
        selected: Vec<SelectedModelRef>,
    },
    OpenModelPicker,
    RefreshCatalog,
    DropVanishedModels {
        model_ids: Vec<ModelId>,
    },
    ShortenPrompt {
        maximum_chars: u32,
    },
    ClearPrompt,
    PinAttachmentRole {
        asset_id: StudioAssetId,
        role: InputRole,
    },
    SwitchMode {
        mode: ComposerMode,
    },
    ResetControl {
        model_id: ModelId,
        control_id: ControlId,
        value: ControlValue,
    },
    DismissWarn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictCode {
    UnsupportedReferences,
    ReferenceCountExceeded,
    MixedReferenceTypes,
    OrphanedAttachments,
    DurationUnsupported,
    DisjointDurations,
    PromptTooLong,
    MissingRequiredInput,
    IncompatibleModeModels,
    EmptyModelSet,
    StaleModel,
    StaleCatalog,
    DisjointCapabilities,
    AttachmentTooLarge,
    AttachmentGeometry,
    AttachmentDuration,
    AudioWithoutVisual,
    OutputCountUnsupported,
    MixedImageVideoIntent,
    QueuePayloadTooLarge,
    ProviderSwitch,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    #[error("action is not offered for this conflict")]
    ActionNotOffered,
}

pub fn conflict_id(code: ConflictCode, subjects: &ConflictSubjects) -> ConflictId {
    let code_key = serde_json::to_value(code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into());
    let mut models = subjects.model_ids.clone();
    models.sort();
    let mut assets = subjects.asset_ids.clone();
    assets.sort();
    let mut controls = subjects.control_ids.clone();
    controls.sort();
    ConflictId(format!(
        "{code_key}:{}:{}:{}",
        models
            .iter()
            .map(|model| model.as_str())
            .collect::<Vec<_>>()
            .join(","),
        assets
            .iter()
            .map(|asset| asset.0.to_string())
            .collect::<Vec<_>>()
            .join(","),
        controls
            .iter()
            .map(|control| control.as_str())
            .collect::<Vec<_>>()
            .join(",")
    ))
}

pub fn popup_conflict(view: &ComposerView, last_event: &ComposerEvent) -> Option<ConflictId> {
    let is_trigger = matches!(
        last_event,
        ComposerEvent::SetMode { .. }
            | ComposerEvent::SelectModel { .. }
            | ComposerEvent::DeselectModel { .. }
            | ComposerEvent::ReplaceModels { .. }
            | ComposerEvent::CatalogUpdated { .. }
            | ComposerEvent::RestoreDraft { .. }
            | ComposerEvent::Attach { .. }
            | ComposerEvent::Detach { .. }
            | ComposerEvent::PinRole { .. }
            | ComposerEvent::Resolve { .. }
    );
    if !is_trigger {
        return None;
    }
    view.conflicts
        .iter()
        .find(|conflict| conflict.blocks_send())
        .map(|conflict| conflict.id.clone())
}

#[allow(clippy::result_large_err)]
pub fn map_tray(
    snapshot: &ComposerSnapshot,
    model: &MediaModel,
) -> Result<Vec<GenerationInput>, ComposerConflict> {
    match model.operation {
        MediaOperation::TextToImage | MediaOperation::ImageToImage => {
            return image_generate_leftovers(snapshot, model);
        }
        MediaOperation::ImageEdit | MediaOperation::Upscale => {
            return Err(incompatible_mode_conflict(std::slice::from_ref(&model)));
        }
        _ => {}
    }

    let committed: Vec<&ComposerAttachment> = snapshot
        .attachments
        .iter()
        .filter(|attachment| !attachment.pending)
        .collect();

    let mut assigned: Vec<Option<InputRole>> = vec![None; committed.len()];
    let mut used: BTreeMap<String, u32> = BTreeMap::new();

    for (index, attachment) in committed.iter().enumerate() {
        let Some(hint) = attachment.role_hint.as_ref() else {
            continue;
        };
        if !hint_is_legal(
            model,
            attachment,
            hint,
            used.get(hint.as_str()).copied().unwrap_or(0),
        ) {
            continue;
        }
        assigned[index] = Some(hint.clone());
        *used.entry(hint.as_str().to_owned()).or_insert(0) += 1;
    }

    let mut overflow = Vec::new();
    let mut leftovers = Vec::new();

    for (index, attachment) in committed.iter().enumerate() {
        if assigned[index].is_some() {
            continue;
        }
        match default_role(model, attachment, &used) {
            DefaultAssign::Role(role) => {
                let count = used.get(role.as_str()).copied().unwrap_or(0);
                let Some(constraint) = role_constraint(model, role.as_str()) else {
                    leftovers.push(index);
                    continue;
                };
                if !mime_accepted(&constraint.mime, &attachment.mime_type) {
                    leftovers.push(index);
                    continue;
                }
                if count >= constraint.maximum_count {
                    overflow.push(index);
                    continue;
                }
                assigned[index] = Some(role.clone());
                *used.entry(role.as_str().to_owned()).or_insert(0) += 1;
            }
            DefaultAssign::Overflow => overflow.push(index),
            DefaultAssign::Leftover => leftovers.push(index),
        }
    }

    if !overflow.is_empty() {
        let mut asset_ids: Vec<StudioAssetId> = overflow.iter().map(|&i| committed[i].id).collect();
        asset_ids.sort_by(|left, right| {
            attach_index(snapshot, right).cmp(&attach_index(snapshot, left))
        });
        return Err(make_conflict(
            ConflictCode::ReferenceCountExceeded,
            format!("{} exceeds its reference limit", model.display_name),
            "Remove the extra references or choose a different model.",
            subjects(vec![model.id.clone()], asset_ids.clone(), Vec::new()),
            vec![
                ResolveAction::RemoveUnsupportedReferences { asset_ids },
                ResolveAction::DeselectIncompatibleModels {
                    model_ids: vec![model.id.clone()],
                },
            ],
        ));
    }

    if !leftovers.is_empty() {
        let leftover_kinds: Vec<ComposerMediaKind> =
            leftovers.iter().map(|&i| committed[i].kind).collect();
        let assigned_kinds: Vec<ComposerMediaKind> = assigned
            .iter()
            .enumerate()
            .filter_map(|(i, role)| role.as_ref().map(|_| committed[i].kind))
            .collect();
        let mixed = !assigned_kinds.is_empty()
            && leftover_kinds
                .iter()
                .any(|kind| !assigned_kinds.contains(kind));
        let asset_ids: Vec<StudioAssetId> = leftovers.iter().map(|&i| committed[i].id).collect();
        let code = if mixed {
            ConflictCode::MixedReferenceTypes
        } else {
            ConflictCode::UnsupportedReferences
        };
        let title = leftover_title(model, &leftover_kinds);
        return Err(make_conflict(
            code,
            title,
            "Remove the leftover attachments or choose a compatible model.",
            subjects(vec![model.id.clone()], asset_ids.clone(), Vec::new()),
            unsupported_actions(model, snapshot, &asset_ids),
        ));
    }

    for constraint in &model.input_constraints {
        if constraint.minimum_count == 0 {
            continue;
        }
        let count = used.get(constraint.role.as_str()).copied().unwrap_or(0);
        if count < constraint.minimum_count {
            return Err(missing_required_conflict(model, snapshot, &constraint.role));
        }
    }

    let visual = used.get(ROLE_SOURCE).copied().unwrap_or(0)
        + used.get(ROLE_REFERENCE).copied().unwrap_or(0)
        + used.get(ROLE_REFERENCE_VIDEO).copied().unwrap_or(0);
    let capability = model.video_capability();
    let audio_refs = used.get(ROLE_REFERENCE_AUDIO).copied().unwrap_or(0);
    if capability
        .as_ref()
        .is_some_and(|cap| cap.reference_audio_requires_visual)
        && audio_refs > 0
        && visual == 0
    {
        let asset_ids: Vec<StudioAssetId> = assigned
            .iter()
            .enumerate()
            .filter_map(|(i, role)| {
                role.as_ref()
                    .is_some_and(|role| role.as_str() == ROLE_REFERENCE_AUDIO)
                    .then_some(committed[i].id)
            })
            .collect();
        return Err(make_conflict(
            ConflictCode::AudioWithoutVisual,
            format!("{} needs a visual reference with audio", model.display_name),
            "Attach an image or video, or remove the audio.",
            subjects(vec![model.id.clone()], asset_ids.clone(), Vec::new()),
            vec![
                ResolveAction::RemoveUnsupportedReferences { asset_ids },
                ResolveAction::DeselectIncompatibleModels {
                    model_ids: vec![model.id.clone()],
                },
            ],
        ));
    }
    if capability
        .as_ref()
        .is_some_and(|cap| cap.requires_visual_reference)
        && visual == 0
    {
        return Err(missing_required_conflict(
            model,
            snapshot,
            &InputRole::from(ROLE_REFERENCE),
        ));
    }

    let mut totals: BTreeMap<String, f64> = BTreeMap::new();
    let mut missing_total: BTreeMap<String, bool> = BTreeMap::new();
    for (index, role) in assigned.iter().enumerate() {
        let Some(role) = role else {
            continue;
        };
        let attachment = committed[index];
        let Some(constraint) = role_constraint(model, role.as_str()) else {
            continue;
        };
        if let Some(conflict) = attachment_constraint_conflict(model, attachment, constraint) {
            return Err(conflict);
        }
        if constraint.mime.maximum_total_duration_seconds.is_some() {
            match attachment.duration_seconds {
                Some(duration) => {
                    *totals.entry(role.as_str().to_owned()).or_insert(0.0) += duration
                }
                None => {
                    missing_total.insert(role.as_str().to_owned(), true);
                }
            }
        }
    }
    for constraint in &model.input_constraints {
        let Some(max_total) = constraint.mime.maximum_total_duration_seconds else {
            continue;
        };
        if missing_total
            .get(constraint.role.as_str())
            .copied()
            .unwrap_or(false)
        {
            return Err(attachment_duration_conflict(model, None, constraint));
        }
        if totals.get(constraint.role.as_str()).copied().unwrap_or(0.0) > max_total {
            return Err(attachment_duration_conflict(model, None, constraint));
        }
    }

    let mut role_ordinals: BTreeMap<String, u32> = BTreeMap::new();
    let mut inputs = Vec::new();
    for (index, role) in assigned.iter().enumerate() {
        let Some(role) = role else {
            continue;
        };
        let attachment = committed[index];
        let ordinal = role_ordinals.entry(role.as_str().to_owned()).or_insert(0);
        inputs.push(generation_input(attachment, role.clone(), *ordinal));
        *ordinal += 1;
    }
    Ok(inputs)
}

enum DefaultAssign {
    Role(InputRole),
    Overflow,
    Leftover,
}

fn default_role(
    model: &MediaModel,
    attachment: &ComposerAttachment,
    used: &BTreeMap<String, u32>,
) -> DefaultAssign {
    match model.operation {
        MediaOperation::TextToVideo => DefaultAssign::Leftover,
        MediaOperation::ImageToVideo => match attachment.kind {
            ComposerMediaKind::Image => i2v_image_role(model, used),
            ComposerMediaKind::Video => optional_role(model, ROLE_REFERENCE_VIDEO, used),
            ComposerMediaKind::Audio => i2v_audio_role(model, used),
        },
        MediaOperation::ReferenceToVideo => match attachment.kind {
            ComposerMediaKind::Image => optional_role(model, ROLE_REFERENCE, used),
            ComposerMediaKind::Video => optional_role(model, ROLE_REFERENCE_VIDEO, used),
            ComposerMediaKind::Audio => optional_role(model, ROLE_REFERENCE_AUDIO, used),
        },
        MediaOperation::VideoToVideo => match attachment.kind {
            ComposerMediaKind::Image => optional_role(model, ROLE_REFERENCE, used),
            ComposerMediaKind::Video => {
                let source_used = used.get(ROLE_SOURCE).copied().unwrap_or(0);
                if source_used == 0 && role_constraint(model, ROLE_SOURCE).is_some() {
                    DefaultAssign::Role(InputRole::from(ROLE_SOURCE))
                } else {
                    optional_role(model, ROLE_REFERENCE_VIDEO, used)
                }
            }
            ComposerMediaKind::Audio => i2v_audio_role(model, used),
        },
        _ => DefaultAssign::Leftover,
    }
}

fn i2v_image_role(model: &MediaModel, used: &BTreeMap<String, u32>) -> DefaultAssign {
    let source_used = used.get(ROLE_SOURCE).copied().unwrap_or(0);
    if source_used == 0 && role_constraint(model, ROLE_SOURCE).is_some() {
        return DefaultAssign::Role(InputRole::from(ROLE_SOURCE));
    }
    let last_used = used.get(ROLE_LAST_FRAME).copied().unwrap_or(0);
    if last_used == 0 && role_max(model, ROLE_LAST_FRAME) > 0 {
        return optional_role(model, ROLE_LAST_FRAME, used);
    }
    optional_role(model, ROLE_REFERENCE, used)
}

fn i2v_audio_role(model: &MediaModel, used: &BTreeMap<String, u32>) -> DefaultAssign {
    if role_max(model, ROLE_AUDIO) > 0 {
        return optional_role(model, ROLE_AUDIO, used);
    }
    optional_role(model, ROLE_REFERENCE_AUDIO, used)
}

fn optional_role(model: &MediaModel, role: &str, used: &BTreeMap<String, u32>) -> DefaultAssign {
    let Some(constraint) = role_constraint(model, role) else {
        return DefaultAssign::Leftover;
    };
    let count = used.get(role).copied().unwrap_or(0);
    if count >= constraint.maximum_count {
        DefaultAssign::Overflow
    } else {
        DefaultAssign::Role(InputRole::from(role))
    }
}

fn hint_is_legal(
    model: &MediaModel,
    attachment: &ComposerAttachment,
    hint: &InputRole,
    used: u32,
) -> bool {
    let Some(constraint) = role_constraint(model, hint.as_str()) else {
        return false;
    };
    if used >= constraint.maximum_count {
        return false;
    }
    if !role_accepts_kind(hint.as_str(), attachment.kind) {
        return false;
    }
    mime_accepted(&constraint.mime, &attachment.mime_type)
}

fn role_constraint<'a>(model: &'a MediaModel, role: &str) -> Option<&'a InputConstraint> {
    model
        .input_constraints
        .iter()
        .find(|constraint| constraint.role.as_str() == role && constraint.maximum_count > 0)
}

fn role_max(model: &MediaModel, role: &str) -> u32 {
    role_constraint(model, role)
        .map(|constraint| constraint.maximum_count)
        .unwrap_or(0)
}

fn role_accepts_kind(role: &str, kind: ComposerMediaKind) -> bool {
    matches!(
        (role, kind),
        (
            ROLE_SOURCE
                | ROLE_LAST_FRAME
                | ROLE_REFERENCE
                | ROLE_ELEMENT
                | ROLE_SCENE
                | ROLE_KEYFRAME,
            ComposerMediaKind::Image,
        ) | (ROLE_SOURCE | ROLE_REFERENCE_VIDEO, ComposerMediaKind::Video)
            | (ROLE_AUDIO | ROLE_REFERENCE_AUDIO, ComposerMediaKind::Audio)
    )
}

fn mime_accepted(constraint: &MimeConstraint, mime: &str) -> bool {
    if constraint.accepted.is_empty() {
        return true;
    }
    constraint.accepted.iter().any(|accepted| {
        accepted.eq_ignore_ascii_case(mime)
            || accepted.ends_with("/*")
                && mime
                    .get(..accepted.len().saturating_sub(1))
                    .is_some_and(|prefix| {
                        prefix.eq_ignore_ascii_case(&accepted[..accepted.len() - 1])
                    })
    })
}

fn attachment_constraint_conflict(
    model: &MediaModel,
    attachment: &ComposerAttachment,
    constraint: &InputConstraint,
) -> Option<ComposerConflict> {
    if !mime_accepted(&constraint.mime, &attachment.mime_type) {
        return Some(make_conflict(
            ConflictCode::UnsupportedReferences,
            leftover_title(model, &[attachment.kind]),
            "Remove the leftover attachments or choose a compatible model.",
            subjects(vec![model.id.clone()], vec![attachment.id], Vec::new()),
            unsupported_actions(model, &ComposerSnapshot::default(), &[attachment.id]),
        ));
    }
    if constraint
        .mime
        .maximum_bytes
        .is_some_and(|maximum| attachment.byte_size > maximum)
    {
        return Some(attachment_code_conflict(
            ConflictCode::AttachmentTooLarge,
            model,
            attachment,
            "This file is larger than the model allows.",
        ));
    }
    if geometry_conflict(&constraint.mime, attachment) {
        return Some(attachment_code_conflict(
            ConflictCode::AttachmentGeometry,
            model,
            attachment,
            "This image does not meet the model's size or aspect requirements.",
        ));
    }
    if duration_conflict(&constraint.mime, attachment) {
        return Some(attachment_duration_conflict(
            model,
            Some(attachment.id),
            constraint,
        ));
    }
    None
}

fn geometry_conflict(mime: &MimeConstraint, attachment: &ComposerAttachment) -> bool {
    let constrained = mime.maximum_width.is_some()
        || mime.maximum_height.is_some()
        || mime.minimum_short_side.is_some()
        || mime.minimum_aspect_ratio.is_some()
        || mime.maximum_aspect_ratio.is_some();
    if !constrained {
        return false;
    }
    let Some(width) = attachment.width else {
        return true;
    };
    let Some(height) = attachment.height else {
        return true;
    };
    if mime.maximum_width.is_some_and(|maximum| width > maximum) {
        return true;
    }
    if mime.maximum_height.is_some_and(|maximum| height > maximum) {
        return true;
    }
    if mime
        .minimum_short_side
        .is_some_and(|minimum| width.min(height) < minimum)
    {
        return true;
    }
    if width == 0 || height == 0 {
        return true;
    }
    let aspect = f64::from(width) / f64::from(height);
    if mime
        .minimum_aspect_ratio
        .is_some_and(|minimum| aspect <= minimum)
    {
        return true;
    }
    if mime
        .maximum_aspect_ratio
        .is_some_and(|maximum| aspect >= maximum)
    {
        return true;
    }
    false
}

fn duration_conflict(mime: &MimeConstraint, attachment: &ComposerAttachment) -> bool {
    let bounded =
        mime.minimum_duration_seconds.is_some() || mime.maximum_duration_seconds.is_some();
    if !bounded {
        return false;
    }
    let Some(duration) = attachment.duration_seconds else {
        return true;
    };
    mime.minimum_duration_seconds
        .is_some_and(|minimum| duration < minimum)
        || mime
            .maximum_duration_seconds
            .is_some_and(|maximum| duration > maximum)
}

fn generation_input(
    attachment: &ComposerAttachment,
    role: InputRole,
    ordinal: u32,
) -> GenerationInput {
    GenerationInput {
        role,
        ordinal,
        source: match &attachment.origin {
            AttachmentOrigin::Asset => GenerationInputSource::Asset {
                asset_id: attachment.id,
            },
            AttachmentOrigin::Artifact { artifact_id } => GenerationInputSource::Artifact {
                artifact_id: *artifact_id,
            },
        },
        content_hash: attachment.content_hash.clone(),
    }
}

pub fn estimate_queue_body_bytes(
    _model: &MediaModel,
    inputs: &[GenerationInput],
    controls: &BTreeMap<ControlId, ControlValue>,
    prompt: &str,
    snapshot: &ComposerSnapshot,
) -> u64 {
    let prompt_json = serde_json::to_vec(prompt)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or(prompt.len() as u64);
    let controls_json = serde_json::to_vec(controls)
        .map(|bytes| bytes.len() as u64)
        .unwrap_or(0);
    let fields = 1 + controls.len() as u64 + inputs.len() as u64;
    let mut total = prompt_json
        .saturating_add(controls_json)
        .saturating_add(fields.saturating_mul(64));
    for input in inputs {
        let raw = attachment_for_input(snapshot, input)
            .map(|attachment| attachment.byte_size)
            .unwrap_or(0);
        total = total.saturating_add(raw.saturating_mul(4) / 3);
    }
    total
}

fn attachment_for_input<'a>(
    snapshot: &'a ComposerSnapshot,
    input: &GenerationInput,
) -> Option<&'a ComposerAttachment> {
    snapshot
        .attachments
        .iter()
        .find(|attachment| match &input.source {
            GenerationInputSource::Asset { asset_id } => attachment.id == *asset_id,
            GenerationInputSource::Artifact { artifact_id } => matches!(
                &attachment.origin,
                AttachmentOrigin::Artifact { artifact_id: origin } if origin == artifact_id
            ),
        })
}

pub fn evaluate_composer(snapshot: &ComposerSnapshot, catalog: &[MediaModel]) -> ComposerView {
    evaluate_composer_inner(snapshot, catalog, ViewFlags::default())
}

#[derive(Clone, Copy, Debug, Default)]
struct ViewFlags {
    open_picker: bool,
    refresh_catalog: bool,
}

fn evaluate_composer_inner(
    snapshot: &ComposerSnapshot,
    catalog: &[MediaModel],
    flags: ViewFlags,
) -> ComposerView {
    let usable = usable_models(snapshot, catalog);
    let mut conflicts = Vec::new();
    collect_selection_conflicts(snapshot, catalog, &mut conflicts);
    collect_duration_conflicts(snapshot, &usable, &mut conflicts);
    collect_prompt_conflict(snapshot, &usable, &mut conflicts);
    collect_output_count_conflicts(snapshot, catalog, &mut conflicts);

    let mut mapped: BTreeMap<String, Vec<GenerationInput>> = BTreeMap::new();
    let mut leftovers_by_model: BTreeMap<String, Vec<StudioAssetId>> = BTreeMap::new();
    for model in mappable_models(snapshot, catalog) {
        match map_tray(snapshot, model) {
            Ok(inputs) => {
                mapped.insert(model.id.as_str().to_owned(), inputs);
            }
            Err(conflict) => {
                if matches!(
                    conflict.code,
                    ConflictCode::UnsupportedReferences | ConflictCode::MixedReferenceTypes
                ) {
                    leftovers_by_model.insert(
                        model.id.as_str().to_owned(),
                        conflict.subjects.asset_ids.clone(),
                    );
                }
                conflicts.push(conflict);
            }
        }
    }
    collect_source_matched_conflicts(snapshot, &usable, &mapped, &mut conflicts);
    collect_image_mode_media_conflicts(snapshot, &mut conflicts);
    collect_orphaned_conflicts(
        snapshot,
        &usable,
        &leftovers_by_model,
        &mapped,
        &mut conflicts,
    );
    collect_queue_conflicts(snapshot, &usable, &mapped, &mut conflicts);

    sort_conflicts(&mut conflicts, snapshot);

    let pending = snapshot
        .attachments
        .iter()
        .any(|attachment| attachment.pending);
    let blocking = conflicts.iter().find(|conflict| conflict.blocks_send());
    let send = SendState {
        enabled: blocking.is_none() && !pending,
        blocked_reason: blocking.map(|conflict| conflict.code),
    };
    let empty_selected = snapshot.selected.is_empty();
    let phase = if blocking.is_some() {
        ComposerPhase::NeedsResolution
    } else if snapshot.prompt.is_empty() && snapshot.attachments.is_empty() {
        ComposerPhase::Idle
    } else {
        ComposerPhase::Editing
    };

    let (duration, duration_choices) = global_duration_view(snapshot, &usable);
    let chips = chip_views(snapshot, catalog, &mapped, &conflicts);
    let budgets = budgets(snapshot, &usable, &mapped);
    let hints = hints(&usable, &mapped, &conflicts);

    ComposerView {
        phase,
        mode: snapshot.mode,
        send,
        globals: GlobalControls {
            duration,
            duration_choices,
        },
        models: chips,
        attachments: AttachmentTrayView {
            items: snapshot.attachments.clone(),
            accept: tray_accept(),
            add_enabled: usable
                .iter()
                .any(|model| model.operation.accepts_reference_assets()),
        },
        budgets,
        hints,
        conflicts,
        catalog_stale: false,
        open_picker: flags.open_picker || empty_selected,
        refresh_catalog: flags.refresh_catalog,
    }
}

pub fn apply_event(
    mut snapshot: ComposerSnapshot,
    catalog: &[MediaModel],
    event: ComposerEvent,
) -> (ComposerSnapshot, ComposerView) {
    let previous_mode = snapshot.mode;
    let previous_selected = snapshot.selected.clone();
    let previous_duration = snapshot.duration.clone();
    let mut flags = ViewFlags::default();
    let inject_mode = matches!(event, ComposerEvent::SetMode { .. });
    let inject_selection = matches!(
        event,
        ComposerEvent::SetMode { .. }
            | ComposerEvent::SelectModel { .. }
            | ComposerEvent::DeselectModel { .. }
            | ComposerEvent::ReplaceModels { .. }
    );

    match event {
        ComposerEvent::SetMode { mode, restore } => {
            snapshot.mode = mode;
            snapshot.selected = filter_restore(mode, restore, catalog);
            dedupe_selected(&mut snapshot.selected);
            match mode {
                ComposerMode::Image => {
                    if snapshot.selected.is_empty() {
                        snapshot.selected = select_first_image_model(catalog);
                    }
                    snapshot.duration = None;
                    strip_duration_from_chips(&mut snapshot);
                }
                ComposerMode::Video => {
                    seed_or_keep_duration(&mut snapshot, catalog);
                    force_video_output_counts(&mut snapshot, catalog);
                    copy_duration_to_chips(&mut snapshot, catalog);
                }
            }
            seed_selected_control_defaults(&mut snapshot, catalog);
        }
        ComposerEvent::SetPrompt { text } => snapshot.prompt = text,
        ComposerEvent::SetDuration { value } => {
            if duration_value_allowed(&snapshot, catalog, &value) {
                snapshot.duration = Some(value);
                copy_duration_to_chips(&mut snapshot, catalog);
            }
        }
        ComposerEvent::Attach { attachment } => {
            if let Some(existing) = snapshot
                .attachments
                .iter_mut()
                .find(|item| item.id == attachment.id)
            {
                *existing = attachment;
            } else {
                snapshot.attachments.push(attachment);
            }
        }
        ComposerEvent::Detach { asset_id } => {
            snapshot.attachments.retain(|item| item.id != asset_id);
        }
        ComposerEvent::PinRole { asset_id, role } => {
            if let Some(attachment) = snapshot
                .attachments
                .iter_mut()
                .find(|item| item.id == asset_id)
            {
                attachment.role_hint = Some(role);
            }
        }
        ComposerEvent::SelectModel {
            provider_id,
            model_id,
        } => {
            if !snapshot
                .selected
                .iter()
                .any(|selected| selected.model_id == model_id)
            {
                let mut selected = SelectedModelRef::new(provider_id, model_id);
                if snapshot.mode == ComposerMode::Video {
                    selected.output_count = 1;
                }
                snapshot.selected.push(selected);
                seed_or_keep_duration(&mut snapshot, catalog);
                force_video_output_counts(&mut snapshot, catalog);
                copy_duration_to_chips(&mut snapshot, catalog);
                seed_selected_control_defaults(&mut snapshot, catalog);
            }
        }
        ComposerEvent::DeselectModel { model_id } => {
            snapshot
                .selected
                .retain(|selected| selected.model_id != model_id);
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
        }
        ComposerEvent::ReplaceModels { selected } => {
            snapshot.selected = selected;
            dedupe_selected(&mut snapshot.selected);
            force_video_output_counts(&mut snapshot, catalog);
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
            seed_selected_control_defaults(&mut snapshot, catalog);
        }
        ComposerEvent::SetModelControl {
            model_id,
            control_id,
            value,
        } => {
            if control_id.as_str() != DURATION_CONTROL
                && let Some(selected) = snapshot
                    .selected
                    .iter_mut()
                    .find(|selected| selected.model_id == model_id)
            {
                selected.controls.insert(control_id, value);
            }
        }
        ComposerEvent::SetOutputCount {
            model_id,
            output_count,
        } => {
            if snapshot.mode != ComposerMode::Video
                && let Some(selected) = snapshot
                    .selected
                    .iter_mut()
                    .find(|selected| selected.model_id == model_id)
            {
                selected.output_count = output_count.max(1);
            }
        }
        ComposerEvent::RestoreDraft { snapshot: restored } => {
            snapshot = restored;
            dedupe_selected(&mut snapshot.selected);
            force_video_output_counts(&mut snapshot, catalog);
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
            seed_selected_control_defaults(&mut snapshot, catalog);
        }
        ComposerEvent::CatalogUpdated { fetched_at } => {
            snapshot.catalog_fetched_at = Some(fetched_at);
        }
        ComposerEvent::Resolve {
            action,
            conflict_id,
        } => {
            let preview = evaluate_composer(&snapshot, catalog);
            if let Some(conflict) = preview
                .conflicts
                .iter()
                .find(|conflict| conflict.id == conflict_id)
                .or_else(|| preview.conflicts.first())
            {
                let mut offered = conflict.clone();
                if matches!(
                    action,
                    ResolveAction::RevertMode { .. } | ResolveAction::RevertModelSelection { .. }
                ) && !offered.actions.iter().any(|view| view.action == action)
                {
                    offered.actions.push(action_view(action.clone()));
                }
                match apply_resolve_checked(snapshot.clone(), catalog, &offered, &action) {
                    Ok(next) => {
                        snapshot = next;
                        if !matches!(
                            action,
                            ResolveAction::OpenModelPicker
                                | ResolveAction::RefreshCatalog
                                | ResolveAction::DismissWarn
                        ) {
                            seed_selected_control_defaults(&mut snapshot, catalog);
                        }
                        flags.open_picker = matches!(action, ResolveAction::OpenModelPicker);
                        flags.refresh_catalog = matches!(action, ResolveAction::RefreshCatalog);
                    }
                    Err(ResolveError::ActionNotOffered) => {}
                }
            }
        }
        ComposerEvent::Send => {}
    }

    force_video_output_counts(&mut snapshot, catalog);

    let mut view = evaluate_composer_inner(&snapshot, catalog, flags);
    if inject_mode {
        inject_revert_mode(
            &mut view,
            previous_mode,
            &previous_selected,
            previous_duration,
        );
    }
    if inject_selection {
        inject_revert_selection(&mut view, previous_selected);
    }
    (snapshot, view)
}

pub fn apply_resolve(
    mut snapshot: ComposerSnapshot,
    catalog: &[MediaModel],
    action: &ResolveAction,
) -> ComposerSnapshot {
    match action {
        ResolveAction::RemoveUnsupportedReferences { asset_ids } => {
            snapshot
                .attachments
                .retain(|attachment| !asset_ids.contains(&attachment.id));
        }
        ResolveAction::RemoveAllAttachments => snapshot.attachments.clear(),
        ResolveAction::DeselectIncompatibleModels { model_ids } => {
            snapshot
                .selected
                .retain(|selected| !model_ids.contains(&selected.model_id));
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
        }
        ResolveAction::KeepModelsDropOthers { model_ids } => {
            snapshot
                .selected
                .retain(|selected| model_ids.contains(&selected.model_id));
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
        }
        ResolveAction::ClampDuration { value } => {
            snapshot.duration = Some(value.clone());
            copy_duration_to_chips(&mut snapshot, catalog);
        }
        ResolveAction::ClearDuration => {
            snapshot.duration = None;
            strip_duration_from_chips(&mut snapshot);
        }
        ResolveAction::RevertMode {
            mode,
            selected,
            duration,
        } => {
            snapshot.mode = *mode;
            snapshot.selected = selected.clone();
            snapshot.duration = duration.clone();
            apply_mode_duration(&mut snapshot, catalog);
        }
        ResolveAction::RevertModelSelection { selected } => {
            snapshot.selected = selected.clone();
            force_video_output_counts(&mut snapshot, catalog);
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
        }
        ResolveAction::OpenModelPicker
        | ResolveAction::RefreshCatalog
        | ResolveAction::DismissWarn => {}
        ResolveAction::DropVanishedModels { model_ids } => {
            snapshot
                .selected
                .retain(|selected| !model_ids.contains(&selected.model_id));
            seed_or_keep_duration(&mut snapshot, catalog);
            copy_duration_to_chips(&mut snapshot, catalog);
        }
        ResolveAction::ShortenPrompt { maximum_chars } => {
            snapshot.prompt = snapshot
                .prompt
                .chars()
                .take(*maximum_chars as usize)
                .collect();
        }
        ResolveAction::ClearPrompt => snapshot.prompt.clear(),
        ResolveAction::PinAttachmentRole { asset_id, role } => {
            if let Some(attachment) = snapshot
                .attachments
                .iter_mut()
                .find(|attachment| attachment.id == *asset_id)
            {
                attachment.role_hint = Some(role.clone());
            }
        }
        ResolveAction::SwitchMode { mode } => {
            snapshot.mode = *mode;
            snapshot.selected.retain(|selected| {
                find_model(catalog, &selected.provider_id, &selected.model_id)
                    .is_some_and(|model| model_matches_mode(model, *mode))
            });
            apply_mode_duration(&mut snapshot, catalog);
        }
        ResolveAction::ResetControl {
            model_id,
            control_id,
            value,
        } => {
            if let Some(selected) = snapshot
                .selected
                .iter_mut()
                .find(|selected| selected.model_id == *model_id)
            {
                if control_id.as_str() == DURATION_CONTROL {
                    snapshot.duration = Some(value.clone());
                    copy_duration_to_chips(&mut snapshot, catalog);
                } else {
                    selected.controls.insert(control_id.clone(), value.clone());
                }
            }
        }
    }
    snapshot
}

pub fn apply_resolve_checked(
    snapshot: ComposerSnapshot,
    catalog: &[MediaModel],
    conflict: &ComposerConflict,
    action: &ResolveAction,
) -> Result<ComposerSnapshot, ResolveError> {
    if !conflict
        .actions
        .iter()
        .any(|offered| &offered.action == action)
    {
        return Err(ResolveError::ActionNotOffered);
    }
    Ok(apply_resolve(snapshot, catalog, action))
}

fn collect_selection_conflicts(
    snapshot: &ComposerSnapshot,
    catalog: &[MediaModel],
    conflicts: &mut Vec<ComposerConflict>,
) {
    if snapshot.selected.is_empty() {
        conflicts.push(make_conflict(
            ConflictCode::EmptyModelSet,
            "Choose a model",
            "Select a model to generate.",
            ConflictSubjects::default(),
            vec![ResolveAction::OpenModelPicker],
        ));
    }

    let mut stale = Vec::new();
    let mut incompatible = Vec::new();
    let mut image_ids = Vec::new();
    let mut video_ids = Vec::new();
    for selected in &snapshot.selected {
        match find_model(catalog, &selected.provider_id, &selected.model_id) {
            None => stale.push(selected.model_id.clone()),
            Some(model) if !model.is_picker_visible() => stale.push(selected.model_id.clone()),
            Some(model)
                if matches!(
                    model.operation,
                    MediaOperation::ImageEdit | MediaOperation::Upscale
                ) =>
            {
                incompatible.push(model);
            }
            Some(model) => match model.output_kind {
                MediaKind::Image => image_ids.push(model.id.clone()),
                MediaKind::Video => video_ids.push(model.id.clone()),
            },
        }
    }
    if !stale.is_empty() {
        conflicts.push(make_conflict(
            ConflictCode::StaleModel,
            "Some models are no longer available",
            "Remove the unavailable models or refresh the catalog.",
            subjects(stale.clone(), Vec::new(), Vec::new()),
            vec![
                ResolveAction::DropVanishedModels { model_ids: stale },
                ResolveAction::RefreshCatalog,
            ],
        ));
    }
    if !incompatible.is_empty() {
        conflicts.push(incompatible_mode_conflict(&incompatible));
    }

    let mixed_outputs = !image_ids.is_empty() && !video_ids.is_empty();
    let mode_mismatch = match snapshot.mode {
        ComposerMode::Image => !video_ids.is_empty(),
        ComposerMode::Video => !image_ids.is_empty(),
    };
    if mixed_outputs || mode_mismatch {
        let drop_ids = match snapshot.mode {
            ComposerMode::Image => video_ids.clone(),
            ComposerMode::Video => image_ids.clone(),
        };
        let switch = match snapshot.mode {
            ComposerMode::Image => ComposerMode::Video,
            ComposerMode::Video => ComposerMode::Image,
        };
        let mut subjects_ids = image_ids;
        subjects_ids.extend(video_ids);
        conflicts.push(make_conflict(
            ConflictCode::MixedImageVideoIntent,
            "Can't mix image and video in one send",
            "Keep the current mode or switch and drop the other models.",
            subjects(subjects_ids, Vec::new(), Vec::new()),
            vec![
                ResolveAction::DeselectIncompatibleModels {
                    model_ids: drop_ids,
                },
                ResolveAction::SwitchMode { mode: switch },
            ],
        ));
    }
}

fn collect_duration_conflicts(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
    conflicts: &mut Vec<ComposerConflict>,
) {
    if snapshot.mode != ComposerMode::Video {
        return;
    }
    let video: Vec<&MediaModel> = usable
        .iter()
        .copied()
        .filter(|model| model.output_kind == MediaKind::Video)
        .collect();
    if video.is_empty() {
        return;
    }
    let popover = duration_popover_models(&video);
    if popover.is_empty() {
        return;
    }
    let auto_only: Vec<ModelId> = video
        .iter()
        .filter(|model| duration_is_auto_only(model))
        .map(|model| model.id.clone())
        .collect();
    let intersection = intersect_video_globals(popover.iter().copied());
    if intersection.durations.is_empty() {
        let mut keep = duration_clique(&popover, snapshot.duration.as_ref());
        keep.extend(auto_only);
        conflicts.push(make_conflict(
            ConflictCode::DisjointDurations,
            "These models don't share a duration",
            "Keep a compatible set of models.",
            subjects(
                popover.iter().map(|model| model.id.clone()).collect(),
                Vec::new(),
                vec![ControlId::from(DURATION_CONTROL)],
            ),
            vec![ResolveAction::KeepModelsDropOthers { model_ids: keep }],
        ));
        return;
    }
    let Some(current) = snapshot.duration.as_ref() else {
        return;
    };
    if duration_allowed(current, &intersection.durations) {
        return;
    }
    let clamp = closest_duration(current, &intersection.durations);
    let lacking: Vec<ModelId> = popover
        .iter()
        .filter(|model| {
            !model
                .video_capability()
                .is_some_and(|cap| cap.durations.iter().any(|choice| choice == current))
        })
        .map(|model| model.id.clone())
        .collect();
    let mut actions = Vec::new();
    if let Some(value) = clamp {
        actions.push(ResolveAction::ClampDuration { value });
    }
    if !lacking.is_empty() {
        actions.push(ResolveAction::DeselectIncompatibleModels {
            model_ids: lacking.clone(),
        });
    }
    conflicts.push(make_conflict(
        ConflictCode::DurationUnsupported,
        "This duration isn't available for every selected model",
        "Clamp to a shared duration or remove the models that cannot use it.",
        subjects(lacking, Vec::new(), vec![ControlId::from(DURATION_CONTROL)]),
        actions,
    ));
}

fn collect_prompt_conflict(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
    conflicts: &mut Vec<ComposerConflict>,
) {
    let prompt_len = snapshot.prompt.chars().count();
    let mut exceeded = Vec::new();
    let mut tightest: Option<u32> = None;
    for model in usable {
        if let Some(maximum) = model.prompt_maximum_chars
            && prompt_len > maximum as usize
        {
            exceeded.push(model.id.clone());
            tightest = Some(tightest.map_or(maximum, |current| current.min(maximum)));
        }
    }
    let Some(maximum_chars) = tightest else {
        return;
    };
    conflicts.push(make_conflict(
        ConflictCode::PromptTooLong,
        "Prompt is too long for a selected model",
        "Shorten the prompt or remove the models with a smaller limit.",
        subjects(exceeded.clone(), Vec::new(), Vec::new()),
        vec![
            ResolveAction::DeselectIncompatibleModels {
                model_ids: exceeded,
            },
            ResolveAction::ShortenPrompt { maximum_chars },
        ],
    ));
}

fn collect_output_count_conflicts(
    snapshot: &ComposerSnapshot,
    catalog: &[MediaModel],
    conflicts: &mut Vec<ComposerConflict>,
) {
    if snapshot.mode == ComposerMode::Video {
        return;
    }
    for selected in &snapshot.selected {
        let Some(model) = find_model(catalog, &selected.provider_id, &selected.model_id) else {
            continue;
        };
        if selected.output_count == 0 || selected.output_count > model.maximum_output_count {
            conflicts.push(make_conflict(
                ConflictCode::OutputCountUnsupported,
                format!("{} does not support that output count", model.display_name),
                "Choose a supported number of outputs.",
                subjects(vec![model.id.clone()], Vec::new(), Vec::new()),
                vec![ResolveAction::DeselectIncompatibleModels {
                    model_ids: vec![model.id.clone()],
                }],
            ));
        }
    }
}

fn collect_source_matched_conflicts(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
    mapped: &BTreeMap<String, Vec<GenerationInput>>,
    conflicts: &mut Vec<ComposerConflict>,
) {
    for model in usable {
        let Some(inputs) = mapped.get(model.id.as_str()) else {
            continue;
        };
        let has_ref_video = inputs
            .iter()
            .any(|input| input.role.as_str() == ROLE_REFERENCE_VIDEO);
        if has_ref_video {
            continue;
        }
        let selected = snapshot
            .selected
            .iter()
            .find(|selected| selected.model_id == model.id);
        let duration_auto = snapshot
            .duration
            .as_ref()
            .is_some_and(|value| matches!(value, ControlValue::DurationAuto))
            || selected.is_some_and(|selected| {
                selected
                    .controls
                    .get(&ControlId::from(DURATION_CONTROL))
                    .is_some_and(|value| matches!(value, ControlValue::DurationAuto))
            });
        let adaptive = selected.is_some_and(|selected| {
            selected
                .controls
                .get(&ControlId::from(ASPECT_CONTROL))
                .is_some_and(|value| matches!(value, ControlValue::AspectRatioAdaptive))
        });
        if !duration_auto && !adaptive {
            continue;
        }
        let (control_id, first_concrete) = if duration_auto {
            (
                ControlId::from(DURATION_CONTROL),
                first_concrete_duration(model),
            )
        } else {
            (
                ControlId::from(ASPECT_CONTROL),
                first_concrete_aspect(model),
            )
        };
        let mut actions = vec![ResolveAction::DeselectIncompatibleModels {
            model_ids: vec![model.id.clone()],
        }];
        if let Some(value) = first_concrete {
            actions.insert(
                0,
                ResolveAction::ResetControl {
                    model_id: model.id.clone(),
                    control_id: control_id.clone(),
                    value,
                },
            );
        }
        conflicts.push(make_conflict(
            ConflictCode::MissingRequiredInput,
            format!(
                "{} needs a reference video for this setting",
                model.display_name
            ),
            "Attach a reference video or reset the source-matched setting.",
            subjects(vec![model.id.clone()], Vec::new(), vec![control_id]),
            actions,
        ));
    }
}

fn collect_image_mode_media_conflicts(
    snapshot: &ComposerSnapshot,
    conflicts: &mut Vec<ComposerConflict>,
) {
    if snapshot.mode != ComposerMode::Image {
        return;
    }
    let asset_ids: Vec<StudioAssetId> = snapshot
        .attachments
        .iter()
        .filter(|attachment| !attachment.pending && attachment.kind != ComposerMediaKind::Image)
        .map(|attachment| attachment.id)
        .collect();
    if asset_ids.is_empty() {
        return;
    }
    conflicts.push(make_conflict(
        ConflictCode::UnsupportedReferences,
        "Image generate does not use video or audio attachments",
        "Remove the leftover media or switch back to video.",
        subjects(Vec::new(), asset_ids.clone(), Vec::new()),
        vec![
            ResolveAction::RemoveUnsupportedReferences { asset_ids },
            ResolveAction::SwitchMode {
                mode: ComposerMode::Video,
            },
        ],
    ));
}

fn collect_orphaned_conflicts(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
    leftovers_by_model: &BTreeMap<String, Vec<StudioAssetId>>,
    mapped: &BTreeMap<String, Vec<GenerationInput>>,
    conflicts: &mut Vec<ComposerConflict>,
) {
    let video_usable: Vec<&MediaModel> = usable
        .iter()
        .copied()
        .filter(|model| model.output_kind == MediaKind::Video)
        .collect();
    if video_usable.is_empty() {
        return;
    }
    let committed: Vec<StudioAssetId> = snapshot
        .attachments
        .iter()
        .filter(|attachment| !attachment.pending)
        .map(|attachment| attachment.id)
        .collect();
    let mut orphaned = Vec::new();
    for asset_id in committed {
        let accepted = mapped.values().any(|inputs| {
            inputs.iter().any(|input| match input.source {
                GenerationInputSource::Asset { asset_id: mapped } => mapped == asset_id,
                GenerationInputSource::Artifact { .. } => false,
            })
        });
        if accepted {
            continue;
        }
        let leftover_everywhere = video_usable.iter().all(|model| {
            leftovers_by_model
                .get(model.id.as_str())
                .is_some_and(|ids| ids.contains(&asset_id))
        });
        if leftover_everywhere {
            orphaned.push(asset_id);
        }
    }
    if orphaned.is_empty() {
        return;
    }
    conflicts.push(make_conflict(
        ConflictCode::OrphanedAttachments,
        "No remaining model can use these attachments",
        "Remove the leftover attachments or restore the previous models.",
        subjects(
            video_usable.iter().map(|model| model.id.clone()).collect(),
            orphaned.clone(),
            Vec::new(),
        ),
        vec![ResolveAction::RemoveUnsupportedReferences {
            asset_ids: orphaned,
        }],
    ));
}

fn collect_queue_conflicts(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
    mapped: &BTreeMap<String, Vec<GenerationInput>>,
    conflicts: &mut Vec<ComposerConflict>,
) {
    for model in usable {
        let Some(inputs) = mapped.get(model.id.as_str()) else {
            continue;
        };
        let selected = snapshot
            .selected
            .iter()
            .find(|selected| selected.model_id == model.id);
        let mut controls = selected
            .map(|selected| selected.controls.clone())
            .unwrap_or_default();
        if let Some(duration) = assigned_duration(snapshot, model) {
            controls.insert(ControlId::from(DURATION_CONTROL), duration);
        }
        let estimate =
            estimate_queue_body_bytes(model, inputs, &controls, &snapshot.prompt, snapshot);
        if estimate <= QUEUE_BODY_LIMIT_BYTES {
            continue;
        }
        let mut sized: Vec<(StudioAssetId, u64)> = inputs
            .iter()
            .filter_map(|input| {
                attachment_for_input(snapshot, input)
                    .map(|attachment| (attachment.id, attachment.byte_size))
            })
            .collect();
        sized.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
        let asset_ids: Vec<StudioAssetId> = sized.into_iter().map(|(id, _)| id).take(1).collect();
        conflicts.push(make_conflict(
            ConflictCode::QueuePayloadTooLarge,
            format!("{} queue payload is too large", model.display_name),
            "Remove the largest attachment or deselect this model. Each run is estimated separately.",
            subjects(vec![model.id.clone()], asset_ids.clone(), Vec::new()),
            vec![
                ResolveAction::RemoveUnsupportedReferences { asset_ids },
                ResolveAction::DeselectIncompatibleModels {
                    model_ids: vec![model.id.clone()],
                },
            ],
        ));
    }
}

fn chip_views(
    snapshot: &ComposerSnapshot,
    catalog: &[MediaModel],
    mapped: &BTreeMap<String, Vec<GenerationInput>>,
    conflicts: &[ComposerConflict],
) -> Vec<ChipView> {
    let mut chips = Vec::new();
    for selected in &snapshot.selected {
        let Some(model) = find_model(catalog, &selected.provider_id, &selected.model_id) else {
            continue;
        };
        let mut values = selected.controls.clone();
        values.retain(|id, _| model.controls.iter().any(|control| &control.id == id));
        for control in &model.controls {
            if control.id.as_str() == DURATION_CONTROL {
                continue;
            }
            if !values.contains_key(&control.id)
                && let Some(default) = &control.default
            {
                values.insert(control.id.clone(), default.clone());
            }
        }
        if snapshot.mode == ComposerMode::Video {
            if let Some(duration) = assigned_duration(snapshot, model) {
                values.insert(ControlId::from(DURATION_CONTROL), duration);
            } else {
                values.remove(&ControlId::from(DURATION_CONTROL));
            }
        }
        let mapped_ok = mapped.contains_key(model.id.as_str());
        let mapped_inputs = mapped.get(model.id.as_str()).cloned().unwrap_or_default();
        let badge = chip_badge(model, &mapped_inputs, mapped_ok, conflicts);
        let output_count =
            if snapshot.mode == ComposerMode::Video || model.output_kind == MediaKind::Video {
                1
            } else {
                selected.output_count
            };
        chips.push(ChipView {
            model_id: model.id.clone(),
            display_name: model.display_name.clone(),
            operation: model.operation,
            output_count,
            controls: model
                .controls
                .iter()
                .filter(|control| control.id.as_str() != DURATION_CONTROL)
                .cloned()
                .collect(),
            values,
            mapped_inputs,
            badge,
        });
    }
    chips
}

fn chip_badge(
    model: &MediaModel,
    _inputs: &[GenerationInput],
    _mapped_ok: bool,
    _conflicts: &[ComposerConflict],
) -> Option<String> {
    if model
        .video_capability()
        .is_some_and(|cap| cap.generate_audio == AudioCapability::None)
    {
        return Some("No audio".to_owned());
    }
    None
}

fn budgets(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
    mapped: &BTreeMap<String, Vec<GenerationInput>>,
) -> Vec<LimitBudget> {
    let mut budgets = Vec::new();
    let used_chars = snapshot.prompt.chars().count() as u32;
    let mut prompt_max: Option<u32> = None;
    let mut prompt_subjects = Vec::new();
    for model in usable {
        if let Some(maximum) = model.prompt_maximum_chars {
            prompt_max = Some(prompt_max.map_or(maximum, |current| current.min(maximum)));
            prompt_subjects.push(model.id.clone());
        }
    }
    budgets.push(LimitBudget {
        kind: BudgetKind::PromptChars,
        used: used_chars,
        maximum: prompt_max,
        remaining: prompt_max.map(|maximum| maximum as i32 - used_chars as i32),
        subjects: prompt_subjects,
    });

    for role in [
        ROLE_SOURCE,
        ROLE_LAST_FRAME,
        ROLE_REFERENCE,
        ROLE_REFERENCE_VIDEO,
        ROLE_REFERENCE_AUDIO,
        ROLE_AUDIO,
    ] {
        let mut maximum: Option<u32> = None;
        let mut subjects = Vec::new();
        for model in usable {
            if let Some(constraint) = role_constraint(model, role) {
                maximum = Some(maximum.map_or(constraint.maximum_count, |current| {
                    current.min(constraint.maximum_count)
                }));
                subjects.push(model.id.clone());
            }
        }
        if subjects.is_empty() {
            continue;
        }
        let used = mapped
            .values()
            .map(|inputs| {
                inputs
                    .iter()
                    .filter(|input| input.role.as_str() == role)
                    .count() as u32
            })
            .max()
            .unwrap_or(0);
        let used = if used == 0 {
            tray_kind_count(snapshot, role, usable)
        } else {
            used
        };
        budgets.push(LimitBudget {
            kind: BudgetKind::Role {
                role: InputRole::from(role),
            },
            used,
            maximum,
            remaining: maximum.map(|max| max as i32 - used as i32),
            subjects,
        });
    }
    budgets
}

fn tray_kind_count(snapshot: &ComposerSnapshot, role: &str, usable: &[&MediaModel]) -> u32 {
    let kind = match role {
        ROLE_SOURCE if source_role_is_video(usable) => ComposerMediaKind::Video,
        ROLE_SOURCE | ROLE_LAST_FRAME | ROLE_REFERENCE => ComposerMediaKind::Image,
        ROLE_REFERENCE_VIDEO => ComposerMediaKind::Video,
        ROLE_AUDIO | ROLE_REFERENCE_AUDIO => ComposerMediaKind::Audio,
        _ => return 0,
    };
    snapshot
        .attachments
        .iter()
        .filter(|attachment| !attachment.pending && attachment.kind == kind)
        .count() as u32
}

fn source_role_is_video(usable: &[&MediaModel]) -> bool {
    usable
        .iter()
        .any(|model| model.operation == MediaOperation::VideoToVideo)
        && !usable
            .iter()
            .any(|model| model.operation == MediaOperation::ImageToVideo)
}

fn hints(
    usable: &[&MediaModel],
    _mapped: &BTreeMap<String, Vec<GenerationInput>>,
    conflicts: &[ComposerConflict],
) -> Vec<LimitHint> {
    let mut hints = Vec::new();
    for model in usable {
        if model.operation == MediaOperation::ImageToVideo
            && conflicts.iter().any(|conflict| {
                conflict.code == ConflictCode::MissingRequiredInput
                    && conflict.subjects.model_ids.contains(&model.id)
                    && conflict.subjects.control_ids.is_empty()
            })
        {
            hints.push(LimitHint {
                text: "Needs a start frame".to_owned(),
                subjects: vec![model.id.clone()],
            });
        }
        if model
            .video_capability()
            .is_some_and(|cap| cap.generate_audio == AudioCapability::None)
        {
            hints.push(LimitHint {
                text: "No audio".to_owned(),
                subjects: vec![model.id.clone()],
            });
        }
    }
    hints
}

fn tray_accept() -> TrayAccept {
    TrayAccept {
        mime_types: crate::STUDIO_INPUT_MIMES
            .iter()
            .map(|mime| (*mime).to_owned())
            .collect(),
    }
}

#[allow(clippy::result_large_err)]
fn image_generate_leftovers(
    snapshot: &ComposerSnapshot,
    model: &MediaModel,
) -> Result<Vec<GenerationInput>, ComposerConflict> {
    let leftover: Vec<&ComposerAttachment> = snapshot
        .attachments
        .iter()
        .filter(|attachment| !attachment.pending && attachment.kind == ComposerMediaKind::Image)
        .collect();
    if leftover.is_empty() {
        return Ok(Vec::new());
    }
    let leftover_kinds: Vec<ComposerMediaKind> = leftover.iter().map(|item| item.kind).collect();
    let asset_ids: Vec<StudioAssetId> = leftover.iter().map(|item| item.id).collect();
    Err(make_conflict(
        ConflictCode::UnsupportedReferences,
        leftover_title(model, &leftover_kinds),
        "Remove the leftover attachments or choose a compatible model.",
        subjects(vec![model.id.clone()], asset_ids.clone(), Vec::new()),
        unsupported_actions(model, snapshot, &asset_ids),
    ))
}

fn usable_models<'a>(
    snapshot: &ComposerSnapshot,
    catalog: &'a [MediaModel],
) -> Vec<&'a MediaModel> {
    snapshot
        .selected
        .iter()
        .filter_map(|selected| find_model(catalog, &selected.provider_id, &selected.model_id))
        .filter(|model| {
            model.is_picker_visible()
                && !matches!(
                    model.operation,
                    MediaOperation::ImageEdit | MediaOperation::Upscale
                )
                && model_matches_mode(model, snapshot.mode)
        })
        .collect()
}

fn mappable_models<'a>(
    snapshot: &ComposerSnapshot,
    catalog: &'a [MediaModel],
) -> Vec<&'a MediaModel> {
    snapshot
        .selected
        .iter()
        .filter_map(|selected| find_model(catalog, &selected.provider_id, &selected.model_id))
        .filter(|model| {
            model.is_picker_visible()
                && !matches!(
                    model.operation,
                    MediaOperation::ImageEdit | MediaOperation::Upscale
                )
        })
        .collect()
}

fn model_matches_mode(model: &MediaModel, mode: ComposerMode) -> bool {
    match mode {
        ComposerMode::Image => {
            model.output_kind == MediaKind::Image
                && !matches!(
                    model.operation,
                    MediaOperation::ImageEdit | MediaOperation::Upscale
                )
        }
        ComposerMode::Video => model.output_kind == MediaKind::Video,
    }
}

fn find_model<'a>(
    catalog: &'a [MediaModel],
    provider_id: &ProviderId,
    model_id: &ModelId,
) -> Option<&'a MediaModel> {
    catalog
        .iter()
        .find(|model| model.id == *model_id && model.provider_id == *provider_id)
        .or_else(|| catalog.iter().find(|model| model.id == *model_id))
}

fn filter_restore(
    mode: ComposerMode,
    restore: Vec<SelectedModelRef>,
    catalog: &[MediaModel],
) -> Vec<SelectedModelRef> {
    restore
        .into_iter()
        .filter(
            |selected| match find_model(catalog, &selected.provider_id, &selected.model_id) {
                Some(model) => model.is_picker_visible() && model_matches_mode(model, mode),
                None => true,
            },
        )
        .collect()
}

fn select_first_image_model(catalog: &[MediaModel]) -> Vec<SelectedModelRef> {
    picker_models(catalog)
        .into_iter()
        .find(|model| model_matches_mode(model, ComposerMode::Image))
        .map(|model| {
            vec![SelectedModelRef::new(
                model.provider_id.clone(),
                model.id.clone(),
            )]
        })
        .unwrap_or_default()
}

fn seed_or_keep_duration(snapshot: &mut ComposerSnapshot, catalog: &[MediaModel]) {
    if snapshot.mode != ComposerMode::Video {
        return;
    }
    let usable = usable_models(snapshot, catalog);
    let video: Vec<&MediaModel> = usable
        .iter()
        .copied()
        .filter(|model| model.output_kind == MediaKind::Video)
        .collect();
    if video.is_empty() {
        return;
    }
    let popover = duration_popover_models(&video);
    if popover.is_empty() {
        snapshot.duration = Some(ControlValue::DurationAuto);
        return;
    }
    let intersection = intersect_video_globals(popover.into_iter());
    if intersection.durations.is_empty() {
        return;
    }
    if snapshot
        .duration
        .as_ref()
        .is_some_and(|current| duration_allowed(current, &intersection.durations))
    {
        return;
    }
    if snapshot.duration.is_none() || matches!(snapshot.duration, Some(ControlValue::DurationAuto))
    {
        snapshot.duration = seed_duration(&intersection.durations);
    }
}

fn apply_mode_duration(snapshot: &mut ComposerSnapshot, catalog: &[MediaModel]) {
    match snapshot.mode {
        ComposerMode::Image => {
            snapshot.duration = None;
            strip_duration_from_chips(snapshot);
        }
        ComposerMode::Video => {
            force_video_output_counts(snapshot, catalog);
            seed_or_keep_duration(snapshot, catalog);
            copy_duration_to_chips(snapshot, catalog);
        }
    }
}

fn seed_duration(choices: &[ControlValue]) -> Option<ControlValue> {
    for prefer in [6.0, 10.0, 5.0] {
        if let Some(found) = choices.iter().find(|choice| {
            matches!(choice, ControlValue::DurationSeconds { value } if (*value - prefer).abs() < f64::EPSILON)
        }) {
            return Some(found.clone());
        }
    }
    let mut seconds: Vec<f64> = choices
        .iter()
        .filter_map(|choice| match choice {
            ControlValue::DurationSeconds { value } => Some(*value),
            _ => None,
        })
        .collect();
    if seconds.is_empty() {
        return choices.first().cloned();
    }
    seconds.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let median = seconds[(seconds.len() - 1) / 2];
    choices.iter().find(|choice| {
        matches!(choice, ControlValue::DurationSeconds { value } if (*value - median).abs() < f64::EPSILON)
    }).cloned()
}

fn closest_duration(current: &ControlValue, choices: &[ControlValue]) -> Option<ControlValue> {
    if matches!(current, ControlValue::DurationAuto) {
        if choices
            .iter()
            .any(|choice| matches!(choice, ControlValue::DurationAuto))
        {
            return Some(ControlValue::DurationAuto);
        }
        return choices
            .iter()
            .filter_map(|choice| match choice {
                ControlValue::DurationSeconds { value } => Some((*value, choice.clone())),
                _ => None,
            })
            .min_by(|left, right| {
                left.0
                    .partial_cmp(&right.0)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(_, choice)| choice);
    }
    let ControlValue::DurationSeconds { value: current } = current else {
        return choices.first().cloned();
    };
    let mut best: Option<(f64, f64, ControlValue)> = None;
    for choice in choices {
        let ControlValue::DurationSeconds { value } = choice else {
            continue;
        };
        let distance = (value - current).abs();
        let better = match &best {
            None => true,
            Some((best_distance, best_value, _)) => {
                distance < *best_distance
                    || ((distance - *best_distance).abs() < f64::EPSILON && value < best_value)
            }
        };
        if better {
            best = Some((distance, *value, choice.clone()));
        }
    }
    best.map(|(_, _, choice)| choice)
}

fn duration_clique(models: &[&MediaModel], current: Option<&ControlValue>) -> Vec<ModelId> {
    let mut groups: Vec<(ControlValue, Vec<ModelId>)> = Vec::new();
    for model in models {
        let Some(capability) = model.video_capability() else {
            continue;
        };
        for duration in &capability.durations {
            if let Some((_, ids)) = groups.iter_mut().find(|(value, _)| value == duration) {
                if !ids.iter().any(|id| id == &model.id) {
                    ids.push(model.id.clone());
                }
            } else {
                groups.push((duration.clone(), vec![model.id.clone()]));
            }
        }
    }
    let chip_index = |id: &ModelId| {
        models
            .iter()
            .position(|model| model.id == *id)
            .unwrap_or(usize::MAX)
    };
    groups.sort_by(|left, right| {
        right
            .1
            .len()
            .cmp(&left.1.len())
            .then_with(|| {
                let left_has = current.is_some_and(|value| &left.0 == value);
                let right_has = current.is_some_and(|value| &right.0 == value);
                right_has.cmp(&left_has)
            })
            .then_with(|| {
                let left_first = left.1.iter().map(chip_index).min().unwrap_or(usize::MAX);
                let right_first = right.1.iter().map(chip_index).min().unwrap_or(usize::MAX);
                left_first.cmp(&right_first)
            })
    });
    groups
        .into_iter()
        .next()
        .map(|(_, mut ids)| {
            ids.sort_by_key(chip_index);
            ids
        })
        .unwrap_or_default()
}

fn duration_allowed(value: &ControlValue, choices: &[ControlValue]) -> bool {
    choices.iter().any(|choice| choice == value)
}

/// True when every advertised duration is Auto — the model has no picker.
pub fn duration_is_auto_only(model: &MediaModel) -> bool {
    model.video_capability().is_some_and(|capability| {
        !capability.durations.is_empty()
            && capability
                .durations
                .iter()
                .all(|duration| matches!(duration, ControlValue::DurationAuto))
    })
}

/// Duration written onto a selected model for evaluate, quote, and queue.
pub fn assigned_duration(snapshot: &ComposerSnapshot, model: &MediaModel) -> Option<ControlValue> {
    if snapshot.mode != ComposerMode::Video || model.output_kind != MediaKind::Video {
        return None;
    }
    if duration_is_auto_only(model) {
        return Some(ControlValue::DurationAuto);
    }
    snapshot.duration.clone()
}

fn duration_popover_models<'a>(video: &[&'a MediaModel]) -> Vec<&'a MediaModel> {
    video
        .iter()
        .copied()
        .filter(|model| !duration_is_auto_only(model))
        .collect()
}

fn duration_value_allowed(
    snapshot: &ComposerSnapshot,
    catalog: &[MediaModel],
    value: &ControlValue,
) -> bool {
    let usable = usable_models(snapshot, catalog);
    let video: Vec<&MediaModel> = usable
        .iter()
        .copied()
        .filter(|model| model.output_kind == MediaKind::Video)
        .collect();
    let popover = duration_popover_models(&video);
    if popover.is_empty() {
        return matches!(value, ControlValue::DurationAuto) && !video.is_empty();
    }
    duration_allowed(
        value,
        &intersect_video_globals(popover.into_iter()).durations,
    )
}

fn global_duration_view(
    snapshot: &ComposerSnapshot,
    usable: &[&MediaModel],
) -> (Option<ControlValue>, Vec<ControlChoice>) {
    if snapshot.mode != ComposerMode::Video {
        return (None, Vec::new());
    }
    let video: Vec<&MediaModel> = usable
        .iter()
        .copied()
        .filter(|model| model.output_kind == MediaKind::Video)
        .collect();
    if video.is_empty() {
        return (snapshot.duration.clone(), Vec::new());
    }
    let popover = duration_popover_models(&video);
    if popover.is_empty() {
        return (
            Some(ControlValue::DurationAuto),
            vec![ControlChoice {
                value: ControlValue::DurationAuto,
                label: "Auto".to_owned(),
            }],
        );
    }
    let intersection = intersect_video_globals(popover.iter().copied());
    (
        snapshot.duration.clone(),
        duration_choices(&popover, &intersection),
    )
}

fn duration_choices(
    usable: &[&MediaModel],
    intersection: &CapabilityIntersection,
) -> Vec<ControlChoice> {
    intersection
        .durations
        .iter()
        .map(|value| {
            let label = usable
                .iter()
                .find_map(|model| {
                    model.controls.iter().find_map(|control| {
                        control
                            .choices
                            .iter()
                            .find(|choice| &choice.value == value)
                            .map(|choice| choice.label.clone())
                    })
                })
                .unwrap_or_else(|| duration_label(value));
            ControlChoice {
                value: value.clone(),
                label,
            }
        })
        .collect()
}

fn duration_label(value: &ControlValue) -> String {
    match value {
        ControlValue::DurationSeconds { value } => {
            if value.fract() == 0.0 {
                format!("{}s", *value as i64)
            } else {
                format!("{value}s")
            }
        }
        ControlValue::DurationAuto => "Auto".to_owned(),
        _ => "Duration".to_owned(),
    }
}

fn first_concrete_duration(model: &MediaModel) -> Option<ControlValue> {
    model.controls.iter().find_map(|control| {
        (control.id.as_str() == DURATION_CONTROL)
            .then_some(())
            .and_then(|_| {
                control.choices.iter().find_map(|choice| {
                    matches!(choice.value, ControlValue::DurationSeconds { .. })
                        .then(|| choice.value.clone())
                })
            })
    })
}

fn first_concrete_aspect(model: &MediaModel) -> Option<ControlValue> {
    model.controls.iter().find_map(|control| {
        (control.id.as_str() == ASPECT_CONTROL)
            .then_some(())
            .and_then(|_| {
                control.choices.iter().find_map(|choice| {
                    matches!(choice.value, ControlValue::AspectRatio { .. })
                        .then(|| choice.value.clone())
                })
            })
    })
}

/// The composer keeps at most one selected entry per model: chips, drafts,
/// and per-model edits all key by model id. A duplicate would display one
/// entry while edits and sends touch another. Keep the first occurrence.
fn dedupe_selected(selected: &mut Vec<SelectedModelRef>) {
    let mut seen = std::collections::HashSet::new();
    selected.retain(|entry| seen.insert(entry.model_id.clone()));
}

fn seed_selected_control_defaults(snapshot: &mut ComposerSnapshot, catalog: &[MediaModel]) {
    for selected in &mut snapshot.selected {
        let Some(model) = find_model(catalog, &selected.provider_id, &selected.model_id) else {
            continue;
        };
        for control in &model.controls {
            if control.id.as_str() == DURATION_CONTROL {
                continue;
            }
            if selected.controls.contains_key(&control.id) {
                continue;
            }
            if let Some(default) = &control.default {
                selected
                    .controls
                    .insert(control.id.clone(), default.clone());
            }
        }
    }
}

fn force_video_output_counts(snapshot: &mut ComposerSnapshot, catalog: &[MediaModel]) {
    if snapshot.mode != ComposerMode::Video {
        return;
    }
    for selected in &mut snapshot.selected {
        selected.output_count = 1;
        if find_model(catalog, &selected.provider_id, &selected.model_id)
            .is_some_and(|model| model.output_kind == MediaKind::Video)
        {
            selected.output_count = 1;
        }
    }
}

fn copy_duration_to_chips(snapshot: &mut ComposerSnapshot, catalog: &[MediaModel]) {
    let assignments: Vec<(usize, Option<ControlValue>)> = snapshot
        .selected
        .iter()
        .enumerate()
        .map(|(index, selected)| {
            let value = find_model(catalog, &selected.provider_id, &selected.model_id)
                .and_then(|model| assigned_duration(snapshot, model))
                .or_else(|| snapshot.duration.clone());
            (index, value)
        })
        .collect();
    for (index, value) in assignments {
        let Some(selected) = snapshot.selected.get_mut(index) else {
            continue;
        };
        if let Some(value) = value {
            selected
                .controls
                .insert(ControlId::from(DURATION_CONTROL), value);
        } else {
            selected.controls.remove(&ControlId::from(DURATION_CONTROL));
        }
    }
}

fn strip_duration_from_chips(snapshot: &mut ComposerSnapshot) {
    for selected in &mut snapshot.selected {
        selected.controls.remove(&ControlId::from(DURATION_CONTROL));
    }
}

fn attach_index(snapshot: &ComposerSnapshot, id: &StudioAssetId) -> usize {
    snapshot
        .attachments
        .iter()
        .position(|attachment| attachment.id == *id)
        .unwrap_or(usize::MAX)
}

fn sort_conflicts(conflicts: &mut [ComposerConflict], snapshot: &ComposerSnapshot) {
    conflicts.sort_by(|left, right| {
        conflict_band(left.code)
            .cmp(&conflict_band(right.code))
            .then_with(|| {
                first_chip_index(snapshot, &left.subjects)
                    .cmp(&first_chip_index(snapshot, &right.subjects))
            })
            .then_with(|| {
                first_attach_index(snapshot, &left.subjects)
                    .cmp(&first_attach_index(snapshot, &right.subjects))
            })
    });
}

fn conflict_band(code: ConflictCode) -> u8 {
    match code {
        ConflictCode::EmptyModelSet => 0,
        ConflictCode::StaleModel => 1,
        ConflictCode::MixedImageVideoIntent => 2,
        ConflictCode::IncompatibleModeModels => 3,
        ConflictCode::UnsupportedReferences
        | ConflictCode::MixedReferenceTypes
        | ConflictCode::ReferenceCountExceeded
        | ConflictCode::OrphanedAttachments
        | ConflictCode::QueuePayloadTooLarge => 4,
        ConflictCode::DurationUnsupported | ConflictCode::DisjointDurations => 5,
        ConflictCode::PromptTooLong => 6,
        ConflictCode::MissingRequiredInput
        | ConflictCode::AudioWithoutVisual
        | ConflictCode::AttachmentTooLarge
        | ConflictCode::AttachmentGeometry
        | ConflictCode::AttachmentDuration => 7,
        _ => 8,
    }
}

fn first_chip_index(snapshot: &ComposerSnapshot, subjects: &ConflictSubjects) -> usize {
    subjects
        .model_ids
        .first()
        .and_then(|id| {
            snapshot
                .selected
                .iter()
                .position(|selected| selected.model_id == *id)
        })
        .unwrap_or(usize::MAX)
}

fn first_attach_index(snapshot: &ComposerSnapshot, subjects: &ConflictSubjects) -> usize {
    subjects
        .asset_ids
        .first()
        .map(|id| attach_index(snapshot, id))
        .unwrap_or(usize::MAX)
}

fn inject_revert_mode(
    view: &mut ComposerView,
    previous: ComposerMode,
    previous_selected: &[SelectedModelRef],
    previous_duration: Option<ControlValue>,
) {
    if view.mode == previous {
        return;
    }
    for conflict in &mut view.conflicts {
        if !conflict.blocks_send() {
            continue;
        }
        if conflict
            .actions
            .iter()
            .any(|action| matches!(action.action, ResolveAction::RevertMode { .. }))
        {
            continue;
        }
        conflict
            .actions
            .push(action_view(ResolveAction::RevertMode {
                mode: previous,
                selected: previous_selected.to_vec(),
                duration: previous_duration.clone(),
            }));
    }
}

fn inject_revert_selection(view: &mut ComposerView, previous: Vec<SelectedModelRef>) {
    if previous.is_empty() {
        return;
    }
    for conflict in &mut view.conflicts {
        if !conflict.blocks_send() {
            continue;
        }
        if !matches!(
            conflict.code,
            ConflictCode::EmptyModelSet
                | ConflictCode::UnsupportedReferences
                | ConflictCode::OrphanedAttachments
                | ConflictCode::MissingRequiredInput
                | ConflictCode::MixedImageVideoIntent
                | ConflictCode::DisjointDurations
                | ConflictCode::DurationUnsupported
                | ConflictCode::StaleModel
        ) {
            continue;
        }
        if conflict
            .actions
            .iter()
            .any(|action| matches!(action.action, ResolveAction::RevertModelSelection { .. }))
        {
            continue;
        }
        conflict
            .actions
            .push(action_view(ResolveAction::RevertModelSelection {
                selected: previous.clone(),
            }));
    }
}

fn unsupported_actions(
    model: &MediaModel,
    snapshot: &ComposerSnapshot,
    asset_ids: &[StudioAssetId],
) -> Vec<ResolveAction> {
    let only = snapshot.selected.len() == 1
        && snapshot
            .selected
            .first()
            .is_some_and(|selected| selected.model_id == model.id);
    let mut actions = Vec::new();
    if !only {
        actions.push(ResolveAction::DeselectIncompatibleModels {
            model_ids: vec![model.id.clone()],
        });
    }
    actions.push(ResolveAction::RemoveUnsupportedReferences {
        asset_ids: asset_ids.to_vec(),
    });
    actions
}

fn leftover_title(model: &MediaModel, kinds: &[ComposerMediaKind]) -> String {
    let has_image = kinds.contains(&ComposerMediaKind::Image);
    let has_video = kinds.contains(&ComposerMediaKind::Video);
    let has_audio = kinds.contains(&ComposerMediaKind::Audio);
    match (has_image, has_video, has_audio) {
        (true, false, false) => format!("{} doesn’t accept reference images", model.display_name),
        (false, true, false) => format!("{} doesn’t accept reference videos", model.display_name),
        (false, false, true) => format!("{} doesn’t accept reference audio", model.display_name),
        _ => format!("{} doesn’t accept these references", model.display_name),
    }
}

fn missing_required_conflict(
    model: &MediaModel,
    snapshot: &ComposerSnapshot,
    role: &InputRole,
) -> ComposerConflict {
    let (title, explanation) = match (role.as_str(), model.operation) {
        (ROLE_SOURCE, MediaOperation::VideoToVideo) => (
            format!("{} needs a source video", model.display_name),
            "Drop or attach a video, or remove this model.",
        ),
        (ROLE_SOURCE, _) => (
            format!("{} needs a start frame", model.display_name),
            "Attach a start frame, or remove this model.",
        ),
        _ => (
            format!("{} needs a visual reference", model.display_name),
            "Attach a compatible reference, or remove this model.",
        ),
    };
    make_conflict(
        ConflictCode::MissingRequiredInput,
        title,
        explanation,
        subjects(vec![model.id.clone()], Vec::new(), Vec::new()),
        missing_required_actions(model, snapshot),
    )
}

fn missing_required_actions(
    model: &MediaModel,
    _snapshot: &ComposerSnapshot,
) -> Vec<ResolveAction> {
    vec![ResolveAction::DeselectIncompatibleModels {
        model_ids: vec![model.id.clone()],
    }]
}

fn incompatible_mode_conflict(models: &[&MediaModel]) -> ComposerConflict {
    let ids: Vec<ModelId> = models.iter().map(|model| model.id.clone()).collect();
    let title = if models.len() == 1 {
        format!("{} can’t be used in this composer", models[0].display_name)
    } else {
        "Some selected models can’t be used in this composer".to_owned()
    };
    make_conflict(
        ConflictCode::IncompatibleModeModels,
        title,
        "Remove the incompatible models.",
        subjects(ids.clone(), Vec::new(), Vec::new()),
        vec![ResolveAction::DeselectIncompatibleModels { model_ids: ids }],
    )
}

fn attachment_code_conflict(
    code: ConflictCode,
    model: &MediaModel,
    attachment: &ComposerAttachment,
    explanation: &str,
) -> ComposerConflict {
    make_conflict(
        code,
        format!("{} rejected an attachment", model.display_name),
        explanation,
        subjects(vec![model.id.clone()], vec![attachment.id], Vec::new()),
        vec![
            ResolveAction::RemoveUnsupportedReferences {
                asset_ids: vec![attachment.id],
            },
            ResolveAction::DeselectIncompatibleModels {
                model_ids: vec![model.id.clone()],
            },
        ],
    )
}

fn attachment_duration_conflict(
    model: &MediaModel,
    asset_id: Option<StudioAssetId>,
    _constraint: &InputConstraint,
) -> ComposerConflict {
    let assets = asset_id.into_iter().collect::<Vec<_>>();
    make_conflict(
        ConflictCode::AttachmentDuration,
        format!("{} rejected an attachment duration", model.display_name),
        "This clip is outside the allowed duration range, or its duration could not be proved.",
        subjects(vec![model.id.clone()], assets.clone(), Vec::new()),
        vec![
            ResolveAction::RemoveUnsupportedReferences { asset_ids: assets },
            ResolveAction::DeselectIncompatibleModels {
                model_ids: vec![model.id.clone()],
            },
        ],
    )
}

fn subjects(
    model_ids: Vec<ModelId>,
    asset_ids: Vec<StudioAssetId>,
    control_ids: Vec<ControlId>,
) -> ConflictSubjects {
    ConflictSubjects {
        model_ids,
        asset_ids,
        control_ids,
    }
}

fn make_conflict(
    code: ConflictCode,
    title: impl Into<String>,
    explanation: impl Into<String>,
    subjects: ConflictSubjects,
    actions: Vec<ResolveAction>,
) -> ComposerConflict {
    let id = conflict_id(code, &subjects);
    ComposerConflict {
        id,
        code,
        severity: ConflictSeverity::BlockSend,
        title: title.into(),
        explanation: explanation.into(),
        subjects,
        actions: actions.into_iter().map(action_view).collect(),
    }
}

fn action_view(action: ResolveAction) -> ResolveActionView {
    ResolveActionView {
        label: action_label(&action),
        action,
    }
}

fn action_label(action: &ResolveAction) -> String {
    match action {
        ResolveAction::RemoveUnsupportedReferences { .. } => "Remove references".to_owned(),
        ResolveAction::RemoveAllAttachments => "Remove all attachments".to_owned(),
        ResolveAction::DeselectIncompatibleModels { model_ids } => {
            if model_ids.len() == 1 {
                "Remove this model".to_owned()
            } else {
                "Remove these models".to_owned()
            }
        }
        ResolveAction::KeepModelsDropOthers { .. } => "Keep compatible models".to_owned(),
        ResolveAction::ClampDuration { value } => format!("Use {}", duration_label(value)),
        ResolveAction::ClearDuration => "Clear duration".to_owned(),
        ResolveAction::RevertMode { .. } => "Undo mode switch".to_owned(),
        ResolveAction::RevertModelSelection { .. } => "Undo model change".to_owned(),
        ResolveAction::OpenModelPicker => "Choose a model".to_owned(),
        ResolveAction::RefreshCatalog => "Refresh models".to_owned(),
        ResolveAction::DropVanishedModels { .. } => "Remove unavailable models".to_owned(),
        ResolveAction::ShortenPrompt { .. } => "Shorten prompt".to_owned(),
        ResolveAction::ClearPrompt => "Clear prompt".to_owned(),
        ResolveAction::PinAttachmentRole { .. } => "Pin role".to_owned(),
        ResolveAction::SwitchMode { mode } => match mode {
            ComposerMode::Image => "Switch to Image".to_owned(),
            ComposerMode::Video => "Switch to Video".to_owned(),
        },
        ResolveAction::ResetControl { .. } => "Reset setting".to_owned(),
        ResolveAction::DismissWarn => "Dismiss".to_owned(),
    }
}
