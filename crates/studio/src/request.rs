use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ControlId, ControlValidationError, ControlValue, InputRole, MediaModel, MediaOperation,
    MediaProbe, MimeConstraint, ModelId, ProviderId,
};

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}

uuid_id!(StudioAssetId);
uuid_id!(StudioArtifactId);
uuid_id!(StudioConversationId);
uuid_id!(StudioTurnId);
uuid_id!(StudioBatchId);
uuid_id!(StudioRunId);
uuid_id!(StudioAttemptId);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum GenerationInputSource {
    Asset { asset_id: StudioAssetId },
    Artifact { artifact_id: StudioArtifactId },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GenerationInput {
    pub role: InputRole,
    pub ordinal: u32,
    pub source: GenerationInputSource,
    pub content_hash: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GenerationRequest {
    pub provider_id: ProviderId,
    pub model_id: ModelId,
    pub operation: MediaOperation,
    pub prompt: String,
    pub negative_prompt: Option<String>,
    pub output_count: u32,
    pub controls: BTreeMap<ControlId, ControlValue>,
    pub inputs: Vec<GenerationInput>,
    /// Snapshot identity of the model this request was validated against.
    /// The engine stamps this from the current catalog after control validation.
    pub manifest_version: String,
    pub display_aspect_ratio: (u32, u32),
}

impl GenerationRequest {
    /// Validate the provider-neutral request before an adapter is allowed to translate it.
    ///
    /// `manifest_version` is not a freshness lock. Call [`Self::bind_to`] so the
    /// persisted snapshot matches the catalog that actually accepted the form.
    pub fn validate_against(&self, model: &MediaModel) -> Result<(), RequestValidationError> {
        if self.provider_id != model.provider_id {
            return Err(RequestValidationError::ProviderMismatch);
        }
        if self.model_id != model.id {
            return Err(RequestValidationError::ModelMismatch);
        }
        if self.operation != model.operation {
            return Err(RequestValidationError::OperationMismatch);
        }
        if self.output_count == 0 || self.output_count > model.maximum_output_count {
            return Err(RequestValidationError::InvalidOutputCount {
                requested: self.output_count,
                maximum: model.maximum_output_count,
            });
        }
        if self.display_aspect_ratio.0 == 0 || self.display_aspect_ratio.1 == 0 {
            return Err(RequestValidationError::InvalidDisplayAspectRatio);
        }
        if model
            .prompt_maximum_chars
            .is_some_and(|maximum| self.prompt.chars().count() > maximum as usize)
        {
            return Err(RequestValidationError::PromptTooLong);
        }
        if let Some(negative_prompt) = &self.negative_prompt
            && model
                .negative_prompt_maximum_chars
                .is_some_and(|maximum| negative_prompt.chars().count() > maximum as usize)
        {
            return Err(RequestValidationError::NegativePromptTooLong);
        }

        model
            .validate_controls(&self.controls)
            .map_err(RequestValidationError::Control)?;
        for control in &model.controls {
            let visible = control.visible_when.iter().all(|condition| {
                self.controls.get(&condition.control_id) == Some(&condition.equals)
            });
            if control.required
                && visible
                && control.default.is_none()
                && !self.controls.contains_key(&control.id)
            {
                return Err(RequestValidationError::MissingControl {
                    control_id: control.id.clone(),
                });
            }
        }

        let mut slots = BTreeSet::new();
        for input in &self.inputs {
            if !slots.insert((input.role.clone(), input.ordinal)) {
                return Err(RequestValidationError::DuplicateInputSlot {
                    role: input.role.clone(),
                    ordinal: input.ordinal,
                });
            }
            if input.content_hash.is_empty() {
                return Err(RequestValidationError::MissingInputHash);
            }
        }
        for constraint in &model.input_constraints {
            let count = self
                .inputs
                .iter()
                .filter(|input| input.role == constraint.role)
                .count() as u32;
            if count < constraint.minimum_count || count > constraint.maximum_count {
                return Err(RequestValidationError::InvalidInputCount {
                    role: constraint.role.clone(),
                    count,
                    minimum: constraint.minimum_count,
                    maximum: constraint.maximum_count,
                });
            }
        }
        if let Some(input) = self.inputs.iter().find(|input| {
            !model
                .input_constraints
                .iter()
                .any(|constraint| constraint.role == input.role)
        }) {
            return Err(RequestValidationError::UnsupportedInputRole {
                role: input.role.clone(),
            });
        }
        Ok(())
    }

    /// Accept the request against `model` and stamp that model's manifest version.
    pub fn bind_to(&mut self, model: &MediaModel) -> Result<(), RequestValidationError> {
        self.validate_against(model)?;
        self.manifest_version = model.manifest_version.clone();
        Ok(())
    }

    /// Drop controls the current catalog does not advertise.
    ///
    /// Reused jobs can carry connection-level or retired keys (for example
    /// `safe_mode` injected after an earlier bind). Those must not fail a new
    /// submit against a catalog that never listed them.
    pub fn drop_unknown_controls(&mut self, model: &MediaModel) {
        self.controls
            .retain(|id, _| model.controls.iter().any(|control| &control.id == id));
    }
}

/// Check sniffed/re-probed bytes against each role's MIME constraint.
/// Snapshot metadata is not trusted: `probes[i]` must describe `inputs[i]`.
pub fn validate_inputs_against_bytes(
    model: &MediaModel,
    inputs: &[GenerationInput],
    probes: &[MediaProbe],
) -> Result<(), RequestValidationError> {
    if inputs.len() != probes.len() {
        return Err(RequestValidationError::InputProbeMismatch);
    }
    let mut role_durations: BTreeMap<&InputRole, (f64, bool)> = BTreeMap::new();
    for (input, probe) in inputs.iter().zip(probes) {
        let Some(constraint) = model
            .input_constraints
            .iter()
            .find(|constraint| constraint.role == input.role)
        else {
            continue;
        };
        validate_probe_against_mime(&constraint.mime, probe, &input.role)?;
        let entry = role_durations.entry(&input.role).or_insert((0.0, false));
        if let Some(duration) = probe.duration_seconds {
            entry.0 += duration;
        } else {
            entry.1 = true;
        }
    }
    for constraint in &model.input_constraints {
        let Some(maximum_total) = constraint.mime.maximum_total_duration_seconds else {
            continue;
        };
        let Some((total, missing)) = role_durations.get(&constraint.role) else {
            continue;
        };
        if *missing || *total > maximum_total {
            return Err(RequestValidationError::InputDuration {
                role: constraint.role.clone(),
            });
        }
    }
    Ok(())
}

fn validate_probe_against_mime(
    mime: &MimeConstraint,
    probe: &MediaProbe,
    role: &InputRole,
) -> Result<(), RequestValidationError> {
    if !mime.accepted.is_empty()
        && !mime
            .accepted
            .iter()
            .any(|accepted| accepted == &probe.mime_type)
    {
        return Err(RequestValidationError::UnsupportedInputMime {
            role: role.clone(),
            mime: probe.mime_type.clone(),
        });
    }
    if mime
        .maximum_bytes
        .is_some_and(|maximum| probe.size_bytes > maximum)
    {
        return Err(RequestValidationError::InputTooLarge { role: role.clone() });
    }
    if geometry_rejected(mime, probe) {
        return Err(RequestValidationError::InputGeometry { role: role.clone() });
    }
    if duration_rejected(mime, probe) {
        return Err(RequestValidationError::InputDuration { role: role.clone() });
    }
    Ok(())
}

fn geometry_rejected(mime: &MimeConstraint, probe: &MediaProbe) -> bool {
    let constrained = mime.maximum_width.is_some()
        || mime.maximum_height.is_some()
        || mime.minimum_short_side.is_some()
        || mime.minimum_aspect_ratio.is_some()
        || mime.maximum_aspect_ratio.is_some();
    if !constrained {
        return false;
    }
    let Some(width) = probe.width else {
        return true;
    };
    let Some(height) = probe.height else {
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
    mime.minimum_aspect_ratio
        .is_some_and(|minimum| aspect <= minimum)
        || mime
            .maximum_aspect_ratio
            .is_some_and(|maximum| aspect >= maximum)
}

fn duration_rejected(mime: &MimeConstraint, probe: &MediaProbe) -> bool {
    let bounded =
        mime.minimum_duration_seconds.is_some() || mime.maximum_duration_seconds.is_some();
    if !bounded {
        return false;
    }
    let Some(duration) = probe.duration_seconds else {
        return true;
    };
    mime.minimum_duration_seconds
        .is_some_and(|minimum| duration < minimum)
        || mime
            .maximum_duration_seconds
            .is_some_and(|maximum| duration > maximum)
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RequestValidationError {
    #[error("request provider does not match model provider")]
    ProviderMismatch,
    #[error("request model id does not match model")]
    ModelMismatch,
    #[error("request operation does not match model operation")]
    OperationMismatch,
    #[error("requested {requested} outputs, model maximum is {maximum}")]
    InvalidOutputCount { requested: u32, maximum: u32 },
    #[error("display aspect ratio must have non-zero dimensions")]
    InvalidDisplayAspectRatio,
    #[error("prompt exceeds model limit")]
    PromptTooLong,
    #[error("negative prompt exceeds model limit")]
    NegativePromptTooLong,
    #[error(transparent)]
    Control(#[from] ControlValidationError),
    #[error("required control {control_id:?} is missing")]
    MissingControl { control_id: ControlId },
    #[error("input slot {role:?}/{ordinal} appears more than once")]
    DuplicateInputSlot { role: InputRole, ordinal: u32 },
    #[error("input content hash must not be empty")]
    MissingInputHash,
    #[error("input role {role:?} has {count} items, expected {minimum}..={maximum}")]
    InvalidInputCount {
        role: InputRole,
        count: u32,
        minimum: u32,
        maximum: u32,
    },
    #[error("input role {role:?} is not supported by the model")]
    UnsupportedInputRole { role: InputRole },
    #[error("input probe count does not match input count")]
    InputProbeMismatch,
    #[error("studio input MIME {mime} is not accepted for role {role:?}")]
    UnsupportedInputMime { role: InputRole, mime: String },
    #[error("studio input exceeds the model size limit")]
    InputTooLarge { role: InputRole },
    #[error("studio input does not meet the model's size or aspect requirements")]
    InputGeometry { role: InputRole },
    #[error("studio input duration is outside the model's allowed range")]
    InputDuration { role: InputRole },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedInput {
    pub role: InputRole,
    pub ordinal: u32,
    pub path: PathBuf,
    pub mime_type: String,
    pub content_hash: String,
    pub size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubmitContext {
    /// Stable for one durable attempt and written before the provider call begins.
    pub idempotency_key: String,
    /// Inputs resolved and hash-verified by the engine immediately before submission.
    pub inputs: Vec<ResolvedInput>,
}
