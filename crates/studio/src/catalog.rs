use crate::{ControlValue, MediaModel};

/// Intersection of selected video models' global settings.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CapabilityIntersection {
    pub durations: Vec<ControlValue>,
    pub prompt_maximum_chars: Option<u32>,
}

/// Duration is the only video global. Resolution, aspect, and audio stay per-model.
pub fn intersect_video_globals<'a>(
    models: impl IntoIterator<Item = &'a MediaModel>,
) -> CapabilityIntersection {
    let mut intersection: Option<CapabilityIntersection> = None;
    for model in models {
        let Some(capability) = model.video_capability() else {
            continue;
        };
        match &mut intersection {
            None => {
                intersection = Some(CapabilityIntersection {
                    durations: capability.durations,
                    prompt_maximum_chars: capability.prompt_maximum_chars,
                });
            }
            Some(current) => {
                current
                    .durations
                    .retain(|duration| capability.durations.iter().any(|other| other == duration));
                current.prompt_maximum_chars = match (
                    current.prompt_maximum_chars,
                    capability.prompt_maximum_chars,
                ) {
                    (Some(left), Some(right)) => Some(left.min(right)),
                    (Some(left), None) => Some(left),
                    (None, right) => right,
                };
            }
        }
    }
    intersection.unwrap_or_default()
}

pub fn picker_models(models: &[MediaModel]) -> Vec<&MediaModel> {
    models
        .iter()
        .filter(|model| model.is_picker_visible())
        .collect()
}
