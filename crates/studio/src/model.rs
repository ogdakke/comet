use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }
    };
}

string_id!(ProviderId);
string_id!(ProviderAccountId);
string_id!(ModelId);
string_id!(ControlId);
string_id!(InputRole);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaKind {
    Image,
    Video,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaOperation {
    TextToImage,
    ImageToImage,
    ImageEdit,
    Upscale,
    TextToVideo,
    ImageToVideo,
    ReferenceToVideo,
    VideoToVideo,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MimeConstraint {
    pub accepted: Vec<String>,
    pub maximum_bytes: Option<u64>,
    pub maximum_width: Option<u32>,
    pub maximum_height: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputConstraint {
    pub role: InputRole,
    pub minimum_count: u32,
    pub maximum_count: u32,
    pub mime: MimeConstraint,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlValue {
    Enum { value: String },
    Integer { value: i64 },
    Number { value: f64 },
    Boolean { value: bool },
    Dimensions { width: u32, height: u32 },
    AspectRatio { width: u32, height: u32 },
    AspectRatioAuto,
    Resolution { value: String },
    DurationSeconds { value: f64 },
}

impl ControlValue {
    pub fn kind(&self) -> ControlKind {
        match self {
            Self::Enum { .. } => ControlKind::Enum,
            Self::Integer { .. } => ControlKind::Integer,
            Self::Number { .. } => ControlKind::Number,
            Self::Boolean { .. } => ControlKind::Boolean,
            Self::Dimensions { .. } => ControlKind::Dimensions,
            Self::AspectRatio { .. } | Self::AspectRatioAuto => ControlKind::AspectRatio,
            Self::Resolution { .. } => ControlKind::Resolution,
            Self::DurationSeconds { .. } => ControlKind::Duration,
        }
    }

    pub fn aspect_ratio_dimensions(&self) -> Option<(u32, u32)> {
        match self {
            Self::AspectRatio { width, height } | Self::Dimensions { width, height } => {
                Some((*width, *height))
            }
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlKind {
    Enum,
    Integer,
    Number,
    Boolean,
    Dimensions,
    AspectRatio,
    Resolution,
    Duration,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ControlChoice {
    pub value: ControlValue,
    pub label: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct VisibilityCondition {
    pub control_id: ControlId,
    pub equals: ControlValue,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ModelControl {
    pub id: ControlId,
    pub label: String,
    pub description: Option<String>,
    pub kind: ControlKind,
    pub required: bool,
    pub default: Option<ControlValue>,
    pub minimum: Option<f64>,
    pub maximum: Option<f64>,
    pub step: Option<f64>,
    pub choices: Vec<ControlChoice>,
    pub visible_when: Vec<VisibilityCondition>,
}

impl ModelControl {
    pub fn validate(&self, value: &ControlValue) -> Result<(), ControlValidationError> {
        if value.kind() != self.kind {
            return Err(ControlValidationError::WrongKind {
                control_id: self.id.clone(),
                expected: self.kind,
                actual: value.kind(),
            });
        }

        let numeric = match value {
            ControlValue::Integer { value } => Some(*value as f64),
            ControlValue::Number { value } | ControlValue::DurationSeconds { value } => {
                Some(*value)
            }
            _ => None,
        };
        if numeric.is_some_and(|value| !value.is_finite()) {
            return Err(ControlValidationError::NonFinite {
                control_id: self.id.clone(),
            });
        }
        if let (Some(minimum), Some(actual)) = (self.minimum, numeric)
            && actual < minimum
        {
            return Err(ControlValidationError::BelowMinimum {
                control_id: self.id.clone(),
                minimum,
                actual,
            });
        }
        if let (Some(maximum), Some(actual)) = (self.maximum, numeric)
            && actual > maximum
        {
            return Err(ControlValidationError::AboveMaximum {
                control_id: self.id.clone(),
                maximum,
                actual,
            });
        }
        if !self.choices.is_empty() && !self.choices.iter().any(|choice| choice.value == *value) {
            return Err(ControlValidationError::UnsupportedChoice {
                control_id: self.id.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, thiserror::Error)]
pub enum ControlValidationError {
    #[error("control {control_id:?} expected {expected:?}, got {actual:?}")]
    WrongKind {
        control_id: ControlId,
        expected: ControlKind,
        actual: ControlKind,
    },
    #[error("control {control_id:?} value {actual} is below minimum {minimum}")]
    BelowMinimum {
        control_id: ControlId,
        minimum: f64,
        actual: f64,
    },
    #[error("control {control_id:?} value {actual} is above maximum {maximum}")]
    AboveMaximum {
        control_id: ControlId,
        maximum: f64,
        actual: f64,
    },
    #[error("control {control_id:?} does not support that choice")]
    UnsupportedChoice { control_id: ControlId },
    #[error("control {control_id:?} must be finite")]
    NonFinite { control_id: ControlId },
    #[error("unknown control {control_id:?}")]
    UnknownControl { control_id: ControlId },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingUnit {
    #[default]
    PerOutput,
    PerRequest,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PricingEntry {
    pub when: BTreeMap<ControlId, ControlValue>,
    pub amount: f64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PricingMetadata {
    pub currency: String,
    #[serde(default)]
    pub unit: PricingUnit,
    /// Retained so older catalog snapshots still deserialize.
    #[serde(default)]
    pub unit_label: String,
    pub amount: Option<f64>,
    #[serde(default)]
    pub entries: Vec<PricingEntry>,
    pub detail: Option<String>,
}

impl PricingMetadata {
    /// True when a catalog claimed pricing exists but recorded no amount.
    /// Pre-cost snapshots used this shape; they must be refetched.
    pub fn is_placeholder(&self) -> bool {
        self.amount.is_none() && self.entries.is_empty()
    }

    fn resolve_unit_amount(&self, controls: &BTreeMap<ControlId, ControlValue>) -> Option<f64> {
        let mut best: Option<(usize, f64)> = None;
        for entry in &self.entries {
            if entry
                .when
                .iter()
                .all(|(id, value)| controls.get(id) == Some(value))
            {
                let specificity = entry.when.len();
                if best.is_none_or(|(current, _)| specificity > current) {
                    best = Some((specificity, entry.amount));
                }
            }
        }
        best.map(|(_, amount)| amount).or(self.amount)
    }
}

/// Provider-advertised capability shown as a picker badge and filter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelFeature {
    Uncensored,
    Anon,
    Private,
}

impl ModelFeature {
    pub const ALL: [Self; 3] = [Self::Uncensored, Self::Anon, Self::Private];

    pub fn label(self) -> &'static str {
        match self {
            Self::Uncensored => "Uncensored",
            Self::Anon => "Anon",
            Self::Private => "Private",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MediaModel {
    pub provider_id: ProviderId,
    pub id: ModelId,
    pub display_name: String,
    pub description: Option<String>,
    pub operation: MediaOperation,
    pub output_kind: MediaKind,
    pub output_mime_types: Vec<String>,
    pub input_constraints: Vec<InputConstraint>,
    pub prompt_maximum_chars: Option<u32>,
    pub negative_prompt_maximum_chars: Option<u32>,
    pub maximum_output_count: u32,
    pub controls: Vec<ModelControl>,
    pub pricing: Option<PricingMetadata>,
    /// Display-only badges. Excluded from [`Self::manifest_version`].
    #[serde(default)]
    pub features: Vec<ModelFeature>,
    /// Changes whenever submit-relevant constraints or controls change.
    /// Display copy and pricing are excluded so a catalog refresh does not
    /// invalidate an otherwise identical form.
    pub manifest_version: String,
    pub fetched_at: DateTime<Utc>,
}

impl MediaModel {
    pub fn validate_controls(
        &self,
        values: &BTreeMap<ControlId, ControlValue>,
    ) -> Result<(), ControlValidationError> {
        for (id, value) in values {
            let control = self
                .controls
                .iter()
                .find(|control| &control.id == id)
                .ok_or_else(|| ControlValidationError::UnknownControl {
                    control_id: id.clone(),
                })?;
            control.validate(value)?;
        }
        Ok(())
    }

    pub fn estimate_cost(
        &self,
        controls: &BTreeMap<ControlId, ControlValue>,
        output_count: u32,
    ) -> Option<crate::Quote> {
        let pricing = self.pricing.as_ref()?;
        let mut effective = BTreeMap::new();
        for control in &self.controls {
            if let Some(value) = controls
                .get(&control.id)
                .cloned()
                .or_else(|| control.default.clone())
            {
                effective.insert(control.id.clone(), value);
            }
        }
        let amount = pricing.resolve_unit_amount(&effective)?;
        if !amount.is_finite() || amount < 0.0 {
            return None;
        }
        let amount = match pricing.unit {
            PricingUnit::PerOutput => amount * f64::from(output_count.max(1)),
            PricingUnit::PerRequest => amount,
        };
        Some(crate::Quote {
            currency: pricing.currency.clone(),
            amount,
            detail: pricing.detail.clone(),
            expires_at: None,
            source: crate::QuoteSource::Catalog,
        })
    }
}
