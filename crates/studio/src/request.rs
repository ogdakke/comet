use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ControlId, ControlValidationError, ControlValue, InputRole, MediaModel, MediaOperation,
    ModelId, ProviderId,
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
    pub manifest_version: String,
    pub display_aspect_ratio: (u32, u32),
}

impl GenerationRequest {
    /// Validate the provider-neutral request before an adapter is allowed to translate it.
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
        if self.manifest_version != model.manifest_version {
            return Err(RequestValidationError::StaleManifest);
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
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum RequestValidationError {
    #[error("request provider does not match model provider")]
    ProviderMismatch,
    #[error("request model id does not match model")]
    ModelMismatch,
    #[error("request operation does not match model operation")]
    OperationMismatch,
    #[error("request manifest version is stale")]
    StaleManifest,
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
