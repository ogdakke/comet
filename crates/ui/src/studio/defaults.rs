//! Sticky last-used Studio models and per-model settings.
//!
//! Same shape as `composer-defaults.json`: a small file beside `ui-settings.json`
//! so the first composer frame can restore the last pick instead of the catalog
//! default. Opening a conversation with turns then overlays that chat's last
//! submitted run.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use zeron_studio::{ComposerMode, ControlValue, ModelId};

use super::draft::{DraftRunConfig, RememberedDraft};

fn default_composer_mode() -> ComposerMode {
    ComposerMode::Image
}

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

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct StudioDefaults {
    pub(super) selected_image_model_ids: Vec<ModelId>,
    pub(super) selected_video_model_ids: Vec<ModelId>,
    /// Pre-video single list. Load maps this onto the image list.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(super) selected_model_ids: Vec<ModelId>,
    pub(super) drafts: BTreeMap<ModelId, RememberedDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) video_duration: Option<ControlValue>,
    #[serde(default = "default_composer_mode")]
    pub(super) last_mode: ComposerMode,
    /// Starred models in the picker, in starring order.
    pub(super) favorites: Vec<ModelId>,
    /// Last-used settings for the artifact viewer's upscale action.
    #[serde(default)]
    pub(super) upscale: UpscaleDefaults,
    /// Last ImageEdit model used from the lightbox composer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) last_edit_model_id: Option<ModelId>,
}

impl Default for StudioDefaults {
    fn default() -> Self {
        Self {
            selected_image_model_ids: Vec::new(),
            selected_video_model_ids: Vec::new(),
            selected_model_ids: Vec::new(),
            drafts: BTreeMap::new(),
            video_duration: None,
            last_mode: ComposerMode::Image,
            favorites: Vec::new(),
            upscale: UpscaleDefaults::default(),
            last_edit_model_id: None,
        }
    }
}

impl StudioDefaults {
    pub(super) fn load(data_dir: &Path) -> Self {
        let mut defaults = match std::fs::read_to_string(Self::path(data_dir)) {
            Ok(text) => match serde_json::from_str::<StudioDefaults>(&text) {
                Ok(defaults) => defaults,
                Err(err) => {
                    tracing::warn!(error = %err, "studio-defaults corrupt; using defaults");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        };
        defaults.migrate_legacy_selection();
        defaults
    }

    fn migrate_legacy_selection(&mut self) {
        if self.selected_image_model_ids.is_empty() && !self.selected_model_ids.is_empty() {
            self.selected_image_model_ids = self.selected_model_ids.clone();
        }
    }

    pub(super) fn selected_ids_for(&self, mode: ComposerMode) -> &[ModelId] {
        match mode {
            ComposerMode::Image => &self.selected_image_model_ids,
            ComposerMode::Video => &self.selected_video_model_ids,
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
        image_ids: &[ModelId],
        video_ids: &[ModelId],
        drafts: &HashMap<ModelId, DraftRunConfig>,
        favorites: &[ModelId],
        upscale: &UpscaleDefaults,
        video_duration: Option<ControlValue>,
        last_mode: ComposerMode,
        last_edit_model_id: Option<ModelId>,
    ) -> Self {
        Self {
            selected_image_model_ids: image_ids.to_vec(),
            selected_video_model_ids: video_ids.to_vec(),
            selected_model_ids: Vec::new(),
            drafts: drafts
                .iter()
                .map(|(id, draft)| {
                    (
                        id.clone(),
                        RememberedDraft {
                            output_count: draft.output_count,
                            controls: super::draft::drop_global_duration(&draft.controls),
                        },
                    )
                })
                .collect(),
            video_duration,
            last_mode,
            favorites: favorites.to_vec(),
            upscale: upscale.clone(),
            last_edit_model_id,
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
        let selected = [ModelId::new("kling"), ModelId::new("flux")];
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
            &[],
            &drafts,
            &[ModelId::new("flux")],
            &UpscaleDefaults::default(),
            None,
            ComposerMode::Image,
            None,
        );
        defaults.save(dir.path()).unwrap();
        assert_eq!(StudioDefaults::load(dir.path()), defaults);
        assert_eq!(
            defaults.selected_image_model_ids,
            vec![ModelId::new("kling"), ModelId::new("flux")]
        );
        assert!(defaults.selected_video_model_ids.is_empty());
        assert_eq!(defaults.favorites, vec![ModelId::new("flux")]);
        assert_eq!(defaults.last_mode, ComposerMode::Image);
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
        assert_eq!(loaded.selected_image_model_ids, vec![ModelId::new("flux")]);
        assert!(loaded.selected_video_model_ids.is_empty());
        assert_eq!(loaded.selected_model_ids, vec![ModelId::new("flux")]);
        assert!(loaded.favorites.is_empty());
        assert_eq!(loaded.upscale, UpscaleDefaults::default());
        assert_eq!(loaded.last_mode, ComposerMode::Image);
    }

    #[test]
    fn per_mode_lists_and_video_duration_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let image = [ModelId::new("flux")];
        let video = [ModelId::new("seedance-t2v")];
        let defaults = StudioDefaults::capture(
            &image,
            &video,
            &HashMap::new(),
            &[],
            &UpscaleDefaults::default(),
            Some(ControlValue::DurationSeconds { value: 6.0 }),
            ComposerMode::Video,
            None,
        );
        defaults.save(dir.path()).unwrap();
        let loaded = StudioDefaults::load(dir.path());
        assert_eq!(loaded.selected_image_model_ids, vec![ModelId::new("flux")]);
        assert_eq!(
            loaded.selected_video_model_ids,
            vec![ModelId::new("seedance-t2v")]
        );
        assert_eq!(
            loaded.video_duration,
            Some(ControlValue::DurationSeconds { value: 6.0 })
        );
        assert_eq!(loaded.last_mode, ComposerMode::Video);
        assert!(loaded.selected_model_ids.is_empty());
    }

    #[test]
    fn capture_drops_duration_from_remembered_drafts() {
        let mut drafts = HashMap::new();
        drafts.insert(
            ModelId::new("seedance-t2v"),
            DraftRunConfig {
                output_count: 1,
                controls: BTreeMap::from([
                    (
                        ControlId::new("duration"),
                        ControlValue::DurationSeconds { value: 8.0 },
                    ),
                    (
                        ControlId::new("resolution"),
                        ControlValue::Resolution {
                            value: "720p".into(),
                        },
                    ),
                ]),
            },
        );
        let defaults = StudioDefaults::capture(
            &[],
            &[ModelId::new("seedance-t2v")],
            &drafts,
            &[],
            &UpscaleDefaults::default(),
            Some(ControlValue::DurationSeconds { value: 8.0 }),
            ComposerMode::Video,
            None,
        );
        let remembered = defaults.drafts.get(&ModelId::new("seedance-t2v")).unwrap();
        assert!(
            !remembered
                .controls
                .contains_key(&ControlId::new("duration"))
        );
        assert_eq!(
            remembered.controls[&ControlId::new("resolution")],
            ControlValue::Resolution {
                value: "720p".into()
            }
        );
        assert_eq!(
            defaults.video_duration,
            Some(ControlValue::DurationSeconds { value: 8.0 })
        );
    }
}
