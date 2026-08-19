//! Reviewed Venice video overlay: inherit, longest-prefix match, role grants.

use crate::{
    AdapterFamily, AudioCapability, ControlChoice, ControlId, ControlKind, ControlValue,
    InputConstraint, InputRole, MediaModel, MediaOperation, MimeConstraint, ModelControl,
    ROLE_LAST_FRAME, ROLE_REFERENCE, ROLE_REFERENCE_AUDIO, ROLE_REFERENCE_VIDEO, VideoModelMeta,
};

const BUNDLED_OVERLAY: &str = include_str!("../overlays/venice/video.toml");

const IMAGE_ACCEPT: &[&str] = &["image/jpeg", "image/png", "image/webp"];
const VIDEO_ACCEPT: &[&str] = &["video/mp4", "video/quicktime"];
const AUDIO_ACCEPT: &[&str] = &["audio/wav", "audio/mpeg"];

#[derive(Debug, thiserror::Error)]
pub enum OverlayError {
    #[error("invalid Venice video overlay TOML: {0}")]
    Toml(#[from] toml::de::Error),
    #[error("overlay row must set exactly one of id or id_prefix")]
    IdXorPrefix,
    #[error("overlay row {key} is missing {field}")]
    MissingField { key: String, field: &'static str },
    #[error("overlay inherit target {inherit} is missing")]
    MissingInherit { inherit: String },
    #[error("overlay inherit cycle involving {key}")]
    Cycle { key: String },
    #[error("overlay has duplicate id or id_prefix {key}")]
    DuplicateKey { key: String },
    #[error("overlay prefix tie between {left} and {right}")]
    PrefixTie { left: String, right: String },
    #[error("overlay promotes {model_id} to reference_to_video without its own reviewed date")]
    UnreviewedPromotion { model_id: String },
}

#[derive(Clone, Debug)]
pub struct VeniceVideoOverlay {
    rows: Vec<ResolvedRow>,
}

/// Public view of one overlay row for catalog drift CI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayRowInfo {
    pub key: String,
    pub exact: bool,
    pub own_reviewed: bool,
    pub source: Option<String>,
    pub operation: Option<MediaOperation>,
}

#[derive(Clone, Debug)]
struct ResolvedRow {
    key: String,
    exact: bool,
    own_reviewed: bool,
    spec: OverlaySpec,
}

impl ResolvedRow {
    fn info(&self) -> OverlayRowInfo {
        OverlayRowInfo {
            key: self.key.clone(),
            exact: self.exact,
            own_reviewed: self.own_reviewed,
            source: self.spec.source.clone(),
            operation: self.spec.operation,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct OverlaySpec {
    source: Option<String>,
    reviewed: Option<String>,
    operation: Option<MediaOperation>,
    adapter_family: Option<AdapterFamily>,
    requires_visual_reference: Option<bool>,
    reference_audio_requires_visual: Option<bool>,
    source_matched_duration: Option<bool>,
    source_matched_aspect: Option<bool>,
    reference_images: Option<CountRange>,
    reference_videos: Option<CountRange>,
    reference_audios: Option<CountRange>,
    last_frame: Option<CountRange>,
    per_reference_video_seconds: Option<BoundRange>,
    total_reference_video_seconds: Option<f64>,
    per_reference_audio_seconds: Option<BoundRange>,
    total_reference_audio_seconds: Option<f64>,
    per_reference_image_bytes: Option<u64>,
    per_reference_video_bytes: Option<u64>,
    per_reference_audio_bytes: Option<u64>,
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
struct CountRange {
    min: u32,
    max: u32,
}

#[derive(Clone, Copy, Debug, serde::Deserialize)]
struct BoundRange {
    min: f64,
    max: f64,
}

#[derive(serde::Deserialize)]
struct OverlayFile {
    #[serde(default)]
    model: Vec<RawRow>,
    #[serde(default)]
    family: Vec<RawRow>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRow {
    id: Option<String>,
    id_prefix: Option<String>,
    inherit: Option<String>,
    source: Option<String>,
    reviewed: Option<String>,
    operation: Option<MediaOperation>,
    adapter_family: Option<AdapterFamily>,
    requires_visual_reference: Option<bool>,
    reference_audio_requires_visual: Option<bool>,
    source_matched_duration: Option<bool>,
    source_matched_aspect: Option<bool>,
    reference_images: Option<CountRange>,
    reference_videos: Option<CountRange>,
    reference_audios: Option<CountRange>,
    last_frame: Option<CountRange>,
    per_reference_video_seconds: Option<BoundRange>,
    total_reference_video_seconds: Option<f64>,
    per_reference_audio_seconds: Option<BoundRange>,
    total_reference_audio_seconds: Option<f64>,
    per_reference_image_bytes: Option<u64>,
    per_reference_video_bytes: Option<u64>,
    per_reference_audio_bytes: Option<u64>,
}

impl RawRow {
    fn into_entry(self, exact: bool) -> Result<(String, bool, Self), OverlayError> {
        match (self.id.as_ref(), self.id_prefix.as_ref(), exact) {
            (Some(id), None, true) if !id.is_empty() => Ok((id.clone(), true, self)),
            (None, Some(prefix), false) if !prefix.is_empty() => Ok((prefix.clone(), false, self)),
            _ => Err(OverlayError::IdXorPrefix),
        }
    }

    fn spec(&self) -> OverlaySpec {
        OverlaySpec {
            source: self.source.clone(),
            reviewed: self.reviewed.clone(),
            operation: self.operation,
            adapter_family: self.adapter_family,
            requires_visual_reference: self.requires_visual_reference,
            reference_audio_requires_visual: self.reference_audio_requires_visual,
            source_matched_duration: self.source_matched_duration,
            source_matched_aspect: self.source_matched_aspect,
            reference_images: self.reference_images,
            reference_videos: self.reference_videos,
            reference_audios: self.reference_audios,
            last_frame: self.last_frame,
            per_reference_video_seconds: self.per_reference_video_seconds,
            total_reference_video_seconds: self.total_reference_video_seconds,
            per_reference_audio_seconds: self.per_reference_audio_seconds,
            total_reference_audio_seconds: self.total_reference_audio_seconds,
            per_reference_image_bytes: self.per_reference_image_bytes,
            per_reference_video_bytes: self.per_reference_video_bytes,
            per_reference_audio_bytes: self.per_reference_audio_bytes,
        }
    }
}

impl OverlaySpec {
    fn overlay(&mut self, other: &Self) {
        if other.source.is_some() {
            self.source = other.source.clone();
        }
        if other.reviewed.is_some() {
            self.reviewed = other.reviewed.clone();
        }
        if other.operation.is_some() {
            self.operation = other.operation;
        }
        if other.adapter_family.is_some() {
            self.adapter_family = other.adapter_family;
        }
        if other.requires_visual_reference.is_some() {
            self.requires_visual_reference = other.requires_visual_reference;
        }
        if other.reference_audio_requires_visual.is_some() {
            self.reference_audio_requires_visual = other.reference_audio_requires_visual;
        }
        if other.source_matched_duration.is_some() {
            self.source_matched_duration = other.source_matched_duration;
        }
        if other.source_matched_aspect.is_some() {
            self.source_matched_aspect = other.source_matched_aspect;
        }
        if other.reference_images.is_some() {
            self.reference_images = other.reference_images;
        }
        if other.reference_videos.is_some() {
            self.reference_videos = other.reference_videos;
        }
        if other.reference_audios.is_some() {
            self.reference_audios = other.reference_audios;
        }
        if other.last_frame.is_some() {
            self.last_frame = other.last_frame;
        }
        if other.per_reference_video_seconds.is_some() {
            self.per_reference_video_seconds = other.per_reference_video_seconds;
        }
        if other.total_reference_video_seconds.is_some() {
            self.total_reference_video_seconds = other.total_reference_video_seconds;
        }
        if other.per_reference_audio_seconds.is_some() {
            self.per_reference_audio_seconds = other.per_reference_audio_seconds;
        }
        if other.total_reference_audio_seconds.is_some() {
            self.total_reference_audio_seconds = other.total_reference_audio_seconds;
        }
        if other.per_reference_image_bytes.is_some() {
            self.per_reference_image_bytes = other.per_reference_image_bytes;
        }
        if other.per_reference_video_bytes.is_some() {
            self.per_reference_video_bytes = other.per_reference_video_bytes;
        }
        if other.per_reference_audio_bytes.is_some() {
            self.per_reference_audio_bytes = other.per_reference_audio_bytes;
        }
    }
}

pub fn bundled_video_overlay() -> Result<VeniceVideoOverlay, OverlayError> {
    VeniceVideoOverlay::parse(BUNDLED_OVERLAY)
}

impl VeniceVideoOverlay {
    pub fn parse(toml: &str) -> Result<Self, OverlayError> {
        let file: OverlayFile = toml::from_str(toml)?;
        let mut entries = Vec::new();
        for row in file.model {
            entries.push(row.into_entry(true)?);
        }
        for row in file.family {
            entries.push(row.into_entry(false)?);
        }

        let mut seen = std::collections::BTreeSet::new();
        for (key, _, _) in &entries {
            if !seen.insert(key.clone()) {
                return Err(OverlayError::DuplicateKey { key: key.clone() });
            }
        }

        let mut rows = Vec::with_capacity(entries.len());
        for (key, exact, row) in &entries {
            let spec = resolve_spec(key, row, &entries, &mut Vec::new())?;
            rows.push(ResolvedRow {
                key: key.clone(),
                exact: *exact,
                own_reviewed: row.reviewed.is_some(),
                spec,
            });
        }
        Ok(Self { rows })
    }

    /// Every loaded row (exact `id` and `id_prefix` families).
    pub fn rows(&self) -> Vec<OverlayRowInfo> {
        self.rows.iter().map(ResolvedRow::info).collect()
    }

    /// Matching overlay row for `model_id` (exact id, else longest prefix).
    pub fn match_info(&self, model_id: &str) -> Result<Option<OverlayRowInfo>, OverlayError> {
        Ok(self.match_model(model_id)?.map(ResolvedRow::info))
    }

    fn match_model(&self, model_id: &str) -> Result<Option<&ResolvedRow>, OverlayError> {
        if let Some(row) = self
            .rows
            .iter()
            .find(|row| row.exact && row.key == model_id)
        {
            return Ok(Some(row));
        }
        let mut best: Option<&ResolvedRow> = None;
        for row in &self.rows {
            if row.exact || !model_id.starts_with(&row.key) {
                continue;
            }
            match best {
                None => best = Some(row),
                Some(current) if row.key.len() > current.key.len() => best = Some(row),
                Some(current) if row.key.len() == current.key.len() => {
                    return Err(OverlayError::PrefixTie {
                        left: current.key.clone(),
                        right: row.key.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        Ok(best)
    }

    /// Overlay family for `model_id`. Unlisted video rows and omitted
    /// `adapter_family` keys stay Hidden — do not guess Seedance from the id.
    pub fn adapter_family(&self, model_id: &str) -> AdapterFamily {
        self.match_model(model_id)
            .ok()
            .flatten()
            .and_then(|row| row.spec.adapter_family)
            .unwrap_or(AdapterFamily::Hidden)
    }

    pub fn apply(
        &self,
        model: &mut MediaModel,
        live_model_type: &str,
        geometry: ImageGeometry,
    ) -> Result<(), OverlayError> {
        let Some(row) = self.match_model(model.id.as_str())? else {
            return Ok(());
        };
        let spec = &row.spec;
        if live_model_type == "image-to-video"
            && spec.operation == Some(MediaOperation::ReferenceToVideo)
            && !row.own_reviewed
        {
            return Err(OverlayError::UnreviewedPromotion {
                model_id: model.id.as_str().to_owned(),
            });
        }
        if let Some(operation) = spec.operation {
            model.operation = operation;
        }
        if model.operation == MediaOperation::ReferenceToVideo {
            model
                .input_constraints
                .retain(|constraint| constraint.role.as_str() != crate::ROLE_SOURCE);
        }

        model.video = VideoModelMeta {
            adapter_family: spec.adapter_family.unwrap_or(AdapterFamily::Hidden),
            requires_visual_reference: spec.requires_visual_reference.unwrap_or(false),
            reference_audio_requires_visual: spec.reference_audio_requires_visual.unwrap_or(false),
            source_matched_duration: spec.source_matched_duration.unwrap_or(false),
            source_matched_aspect: spec.source_matched_aspect.unwrap_or(false),
            generate_audio: model.video.generate_audio,
        };
        if model.video.adapter_family == AdapterFamily::Grok {
            model.video.generate_audio = AudioCapability::None;
            model
                .controls
                .retain(|control| control.id.as_str() != "audio");
        }

        if let Some(range) = spec.last_frame {
            upsert_role(
                model,
                ROLE_LAST_FRAME,
                range,
                image_mime(spec.per_reference_image_bytes, geometry),
            );
        }
        if let Some(range) = spec.reference_images {
            upsert_role(
                model,
                ROLE_REFERENCE,
                range,
                image_mime(spec.per_reference_image_bytes, geometry),
            );
        }
        if let Some(range) = spec.reference_videos {
            let mut mime = MimeConstraint::accepting(VIDEO_ACCEPT.iter().copied());
            mime.maximum_bytes = spec.per_reference_video_bytes;
            if let Some(bounds) = spec.per_reference_video_seconds {
                mime.minimum_duration_seconds = Some(bounds.min);
                mime.maximum_duration_seconds = Some(bounds.max);
            }
            mime.maximum_total_duration_seconds = spec.total_reference_video_seconds;
            upsert_role(model, ROLE_REFERENCE_VIDEO, range, mime);
        }
        if let Some(range) = spec.reference_audios {
            let mut mime = MimeConstraint::accepting(AUDIO_ACCEPT.iter().copied());
            mime.maximum_bytes = spec.per_reference_audio_bytes;
            if let Some(bounds) = spec.per_reference_audio_seconds {
                mime.minimum_duration_seconds = Some(bounds.min);
                mime.maximum_duration_seconds = Some(bounds.max);
            }
            mime.maximum_total_duration_seconds = spec.total_reference_audio_seconds;
            upsert_role(model, ROLE_REFERENCE_AUDIO, range, mime);
        }

        if model.video.source_matched_duration {
            insert_duration_auto(model);
        }
        if model.video.source_matched_aspect {
            insert_aspect_adaptive(model);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ImageGeometry {
    pub minimum_short_side: Option<u32>,
    pub minimum_aspect_ratio: Option<f64>,
    pub maximum_aspect_ratio: Option<f64>,
}

fn image_mime(maximum_bytes: Option<u64>, geometry: ImageGeometry) -> MimeConstraint {
    let mut mime = MimeConstraint::accepting(IMAGE_ACCEPT.iter().copied());
    mime.maximum_bytes = maximum_bytes;
    mime.minimum_short_side = geometry.minimum_short_side;
    mime.minimum_aspect_ratio = geometry.minimum_aspect_ratio;
    mime.maximum_aspect_ratio = geometry.maximum_aspect_ratio;
    mime
}

fn upsert_role(model: &mut MediaModel, role: &str, range: CountRange, mime: MimeConstraint) {
    if let Some(existing) = model
        .input_constraints
        .iter_mut()
        .find(|constraint| constraint.role.as_str() == role)
    {
        existing.minimum_count = range.min;
        existing.maximum_count = range.max;
        existing.mime = mime;
        return;
    }
    model.input_constraints.push(InputConstraint {
        role: InputRole::from(role),
        minimum_count: range.min,
        maximum_count: range.max,
        mime,
    });
}

fn insert_duration_auto(model: &mut MediaModel) {
    let auto = ControlChoice {
        value: ControlValue::DurationAuto,
        label: "Auto".to_owned(),
    };
    if let Some(control) = model
        .controls
        .iter_mut()
        .find(|control| control.id.as_str() == "duration")
    {
        if !control
            .choices
            .iter()
            .any(|choice| choice.value == ControlValue::DurationAuto)
        {
            control.choices.push(auto);
        }
        return;
    }
    model.controls.push(ModelControl {
        id: ControlId::from("duration"),
        label: "Duration".to_owned(),
        description: None,
        kind: ControlKind::Duration,
        required: true,
        default: Some(ControlValue::DurationAuto),
        minimum: None,
        maximum: None,
        step: None,
        choices: vec![auto],
        visible_when: Vec::new(),
    });
}

fn insert_aspect_adaptive(model: &mut MediaModel) {
    let adaptive = ControlChoice {
        value: ControlValue::AspectRatioAdaptive,
        label: "Adaptive".to_owned(),
    };
    if let Some(control) = model
        .controls
        .iter_mut()
        .find(|control| control.id.as_str() == "aspect_ratio")
    {
        if !control
            .choices
            .iter()
            .any(|choice| choice.value == ControlValue::AspectRatioAdaptive)
        {
            control.choices.push(adaptive);
        }
        return;
    }
    model.controls.push(ModelControl {
        id: ControlId::from("aspect_ratio"),
        label: "Aspect ratio".to_owned(),
        description: None,
        kind: ControlKind::AspectRatio,
        required: false,
        default: Some(ControlValue::AspectRatioAdaptive),
        minimum: None,
        maximum: None,
        step: None,
        choices: vec![adaptive],
        visible_when: Vec::new(),
    });
}

fn resolve_spec(
    key: &str,
    row: &RawRow,
    entries: &[(String, bool, RawRow)],
    stack: &mut Vec<String>,
) -> Result<OverlaySpec, OverlayError> {
    if stack.iter().any(|seen| seen == key) {
        return Err(OverlayError::Cycle {
            key: key.to_owned(),
        });
    }
    stack.push(key.to_owned());
    let spec = match row.inherit.as_deref() {
        None => {
            if row.source.is_none() {
                return Err(OverlayError::MissingField {
                    key: key.to_owned(),
                    field: "source",
                });
            }
            if row.reviewed.is_none() {
                return Err(OverlayError::MissingField {
                    key: key.to_owned(),
                    field: "reviewed",
                });
            }
            if row.operation.is_none() {
                return Err(OverlayError::MissingField {
                    key: key.to_owned(),
                    field: "operation",
                });
            }
            row.spec()
        }
        Some(inherit) => {
            let parent = entries
                .iter()
                .find(|(parent_key, _, _)| parent_key == inherit)
                .ok_or_else(|| OverlayError::MissingInherit {
                    inherit: inherit.to_owned(),
                })?;
            let mut spec = resolve_spec(&parent.0, &parent.2, entries, stack)?;
            spec.overlay(&row.spec());
            spec
        }
    };
    stack.pop();
    if spec.operation.is_none() {
        return Err(OverlayError::MissingField {
            key: key.to_owned(),
            field: "operation",
        });
    }
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MediaKind, ModelId, ProviderId};
    use chrono::Utc;

    const HEADER: &str = r#"
source = "https://docs.venice.ai/guides/media/seedance-2-0"
reviewed = "2026-08-18"
"#;

    fn video_model(id: &str) -> MediaModel {
        MediaModel {
            provider_id: ProviderId::from("venice"),
            id: ModelId::from(id),
            display_name: id.to_owned(),
            description: None,
            operation: MediaOperation::ImageToVideo,
            output_kind: MediaKind::Video,
            output_mime_types: vec!["video/mp4".into()],
            input_constraints: vec![InputConstraint {
                role: InputRole::from(crate::ROLE_SOURCE),
                minimum_count: 1,
                maximum_count: 1,
                mime: MimeConstraint::accepting(["image/png"]),
            }],
            prompt_maximum_chars: Some(2500),
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls: Vec::new(),
            pricing: None,
            features: Vec::new(),
            video: VideoModelMeta::default(),
            manifest_version: String::new(),
            fetched_at: Utc::now(),
        }
    }

    #[test]
    fn bundled_overlay_loads() {
        bundled_video_overlay().unwrap();
    }

    #[test]
    fn adapter_family_uses_overlay_and_defaults_hidden() {
        let overlay = bundled_video_overlay().unwrap();
        assert_eq!(
            overlay.adapter_family("seedance-1-5-pro-text-to-video-basic"),
            AdapterFamily::Seedance
        );
        assert_eq!(
            overlay.adapter_family("grok-imagine-reference-to-video-private"),
            AdapterFamily::Grok
        );
        assert_eq!(
            overlay.adapter_family("kling-o3-pro-reference-to-video"),
            AdapterFamily::Hidden
        );
        assert_eq!(
            overlay.adapter_family("seedance-2-0-text-to-video-basic"),
            AdapterFamily::Seedance
        );
        assert_eq!(
            overlay.adapter_family("seedance-2-5-text-to-video-basic"),
            AdapterFamily::Seedance
        );
    }

    #[test]
    fn inherit_copies_then_overrides() {
        let overlay = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "parent"
operation = "reference_to_video"
adapter_family = "seedance"
{HEADER}
reference_images = {{ min = 0, max = 9 }}
source_matched_aspect = true

[[family]]
id_prefix = "child"
inherit = "parent"
reviewed = "2026-08-18"
reference_images = {{ min = 0, max = 30 }}
source_matched_duration = true
"#
        ))
        .unwrap();
        let mut model = video_model("child-model");
        overlay
            .apply(&mut model, "image-to-video", ImageGeometry::default())
            .unwrap();
        assert_eq!(model.operation, MediaOperation::ReferenceToVideo);
        assert_eq!(model.video.adapter_family, AdapterFamily::Seedance);
        assert!(model.video.source_matched_aspect);
        assert!(model.video.source_matched_duration);
        let references = model
            .input_constraints
            .iter()
            .find(|constraint| constraint.role.as_str() == ROLE_REFERENCE)
            .unwrap();
        assert_eq!(references.maximum_count, 30);
    }

    #[test]
    fn missing_inherit_target_is_a_load_error() {
        let error = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "child"
inherit = "missing"
{HEADER}
"#
        ))
        .unwrap_err();
        assert!(matches!(error, OverlayError::MissingInherit { .. }));
    }

    #[test]
    fn inherit_cycles_are_a_load_error() {
        let error = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "a"
inherit = "b"
{HEADER}

[[family]]
id_prefix = "b"
inherit = "a"
{HEADER}
"#
        ))
        .unwrap_err();
        assert!(matches!(error, OverlayError::Cycle { .. }));
    }

    #[test]
    fn longest_prefix_wins() {
        let overlay = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "seedance"
operation = "text_to_video"
adapter_family = "hidden"
{HEADER}

[[family]]
id_prefix = "seedance-2-5-reference-to-video"
operation = "reference_to_video"
adapter_family = "seedance"
{HEADER}
"#
        ))
        .unwrap();
        let mut model = video_model("seedance-2-5-reference-to-video-basic");
        overlay
            .apply(&mut model, "image-to-video", ImageGeometry::default())
            .unwrap();
        assert_eq!(model.operation, MediaOperation::ReferenceToVideo);
        assert_eq!(model.video.adapter_family, AdapterFamily::Seedance);
    }

    #[test]
    fn equal_length_prefix_tie_is_a_load_error() {
        let error = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "same-prefix"
operation = "text_to_video"
adapter_family = "hidden"
{HEADER}

[[family]]
id_prefix = "same-prefix"
operation = "reference_to_video"
adapter_family = "seedance"
{HEADER}
"#
        ))
        .unwrap_err();
        assert!(matches!(
            error,
            OverlayError::DuplicateKey { .. } | OverlayError::PrefixTie { .. }
        ));
    }

    #[test]
    fn unreviewed_i2v_promotion_is_an_error() {
        let overlay = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "parent"
operation = "reference_to_video"
adapter_family = "seedance"
{HEADER}

[[family]]
id_prefix = "child"
inherit = "parent"
"#
        ))
        .unwrap();
        let mut model = video_model("child-model");
        let error = overlay
            .apply(&mut model, "image-to-video", ImageGeometry::default())
            .unwrap_err();
        assert!(matches!(error, OverlayError::UnreviewedPromotion { .. }));
    }

    #[test]
    fn exact_id_wins_over_prefix() {
        let overlay = VeniceVideoOverlay::parse(&format!(
            r#"
[[family]]
id_prefix = "seedance"
operation = "text_to_video"
adapter_family = "hidden"
{HEADER}

[[model]]
id = "seedance-special"
operation = "image_to_video"
adapter_family = "seedance"
{HEADER}
"#
        ))
        .unwrap();
        let mut model = video_model("seedance-special");
        overlay
            .apply(&mut model, "image-to-video", ImageGeometry::default())
            .unwrap();
        assert_eq!(model.operation, MediaOperation::ImageToVideo);
        assert_eq!(model.video.adapter_family, AdapterFamily::Seedance);
    }
}
