//! Sticky last-used Studio models and per-model settings.
//!
//! Same shape as `composer-defaults.json`: a small file beside `ui-settings.json`
//! so the first composer frame can restore the last pick instead of the catalog
//! default. Opening a conversation with turns then overlays that chat's last
//! submitted run.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeron_studio::ModelId;

use super::draft::{DraftRunConfig, RememberedDraft};

const FILE_NAME: &str = "studio-defaults.json";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct UpscaleDefaults {
    /// The output multiplier offered by the provider's upscale model.
    pub(super) scale: i64,
    /// Provider-specific creativity amount. Venice accepts 0.0 through 0.02.
    pub(super) creativity: f64,
}

impl Default for UpscaleDefaults {
    fn default() -> Self {
        Self {
            scale: 4,
            creativity: 0.0,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct StudioDefaults {
    pub(super) selected_model_ids: Vec<ModelId>,
    pub(super) drafts: BTreeMap<ModelId, RememberedDraft>,
    /// Starred models in the picker, in starring order.
    pub(super) favorites: Vec<ModelId>,
    /// Last-used settings for the artifact viewer's upscale action.
    #[serde(default)]
    pub(super) upscale: UpscaleDefaults,
}

impl StudioDefaults {
    pub(super) fn load(data_dir: &Path) -> Self {
        match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<StudioDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "studio-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub(super) fn save(&self, data_dir: &Path) -> io::Result<()> {
        std::fs::create_dir_all(data_dir)?;
        let path = Self::path(data_dir);
        let tmp = path.with_extension("json.tmp");
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, &path)
    }

    pub(super) fn path(data_dir: &Path) -> PathBuf {
        data_dir.join(FILE_NAME)
    }

    pub(super) fn capture(
        selected: &BTreeSet<ModelId>,
        drafts: &HashMap<ModelId, DraftRunConfig>,
        favorites: &[ModelId],
        upscale: &UpscaleDefaults,
    ) -> Self {
        Self {
            selected_model_ids: selected.iter().cloned().collect(),
            drafts: drafts
                .iter()
                .map(|(id, draft)| {
                    (
                        id.clone(),
                        RememberedDraft {
                            output_count: draft.output_count,
                            controls: draft.controls.clone(),
                        },
                    )
                })
                .collect(),
            favorites: favorites.to_vec(),
            upscale: upscale.clone(),
        }
    }

    pub(super) fn is_favorite(&self, model: &ModelId) -> bool {
        self.favorites.iter().any(|id| id == model)
    }

    /// Star/unstar a model; returns whether it is starred AFTER the toggle.
    pub(super) fn toggle_favorite(&mut self, model: &ModelId) -> bool {
        if let Some(at) = self.favorites.iter().position(|id| id == model) {
            self.favorites.remove(at);
            false
        } else {
            self.favorites.push(model.clone());
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeron_studio::{ControlId, ControlValue};

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut selected = BTreeSet::new();
        selected.insert(ModelId::new("flux"));
        selected.insert(ModelId::new("kling"));
        let mut drafts = HashMap::new();
        drafts.insert(
            ModelId::new("flux"),
            DraftRunConfig {
                output_count: 4,
                controls: BTreeMap::from([(
                    ControlId::new("aspect_ratio"),
                    ControlValue::AspectRatio {
                        width: 16,
                        height: 9,
                    },
                )]),
            },
        );
        let defaults = StudioDefaults::capture(
            &selected,
            &drafts,
            &[ModelId::new("flux")],
            &UpscaleDefaults::default(),
        );
        defaults.save(dir.path()).unwrap();
        assert_eq!(StudioDefaults::load(dir.path()), defaults);
        assert_eq!(
            defaults.selected_model_ids,
            vec![ModelId::new("flux"), ModelId::new("kling")]
        );
        assert_eq!(defaults.favorites, vec![ModelId::new("flux")]);
    }

    #[test]
    fn favorites_toggle_and_persist() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = StudioDefaults::default();
        assert!(defaults.toggle_favorite(&ModelId::new("flux")));
        assert!(defaults.toggle_favorite(&ModelId::new("kling")));
        assert!(defaults.is_favorite(&ModelId::new("flux")));
        defaults.save(dir.path()).unwrap();
        assert_eq!(StudioDefaults::load(dir.path()), defaults);
        assert!(!defaults.toggle_favorite(&ModelId::new("flux")));
        assert!(!defaults.is_favorite(&ModelId::new("flux")));
        assert!(defaults.is_favorite(&ModelId::new("kling")));
    }

    #[test]
    fn upscale_settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut defaults = StudioDefaults::default();
        defaults.upscale = UpscaleDefaults {
            scale: 4,
            creativity: 0.007,
        };
        defaults.save(dir.path()).unwrap();
        assert_eq!(StudioDefaults::load(dir.path()).upscale, defaults.upscale);
    }

    #[test]
    fn missing_and_corrupt_files_yield_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(StudioDefaults::load(dir.path()), StudioDefaults::default());
        std::fs::write(StudioDefaults::path(dir.path()), "{nope").unwrap();
        assert_eq!(StudioDefaults::load(dir.path()), StudioDefaults::default());
        std::fs::write(
            StudioDefaults::path(dir.path()),
            r#"{"selectedModelIds":["flux"],"drafts":{}}"#,
        )
        .unwrap();
        let loaded = StudioDefaults::load(dir.path());
        assert_eq!(loaded.selected_model_ids, vec![ModelId::new("flux")]);
        assert!(loaded.favorites.is_empty());
        assert_eq!(loaded.upscale, UpscaleDefaults::default());
    }
}
