//! Overlay / swagger key-drift CI.
//!
//! Fails the build when:
//! - a live fixture constraint key is unknown to the parser
//! - a swagger Video Model Constraints key is unknown to the parser
//! - a live `image-to-video` row is promoted to R2V without that overlay
//!   row's own `reviewed`
//! - a T2V fixture gains any `reference_*` input after overlay apply
//! - an overlay `source` is not a listed URL or `live fixture + swagger`

use chrono::{TimeZone, Utc};
use zeron_studio::{
    MediaOperation,
    venice::{
        ALLOWED_OVERLAY_SOURCES, SWAGGER_VIDEO_CONSTRAINT_KEYS, VIDEO_CONSTRAINT_KEYS,
        normalize_model_catalog, unknown_video_constraint_keys,
    },
    venice_overlay::bundled_video_overlay,
};

const TEXT_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/text-to-video-model.json");
const IMAGE_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/image-to-video-model.json");
const SEEDANCE_2_5_R2V: &[u8] =
    include_bytes!("fixtures/venice/seedance-2-5-reference-to-video-model.json");

fn fetched_at() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_777_000_000, 0).unwrap()
}

fn live_video_fixtures() -> [&'static [u8]; 3] {
    [TEXT_TO_VIDEO, IMAGE_TO_VIDEO, SEEDANCE_2_5_R2V]
}

#[test]
fn unknown_constraint_key_is_reported() {
    let unknown = unknown_video_constraint_keys(&serde_json::json!({
        "model_type": "text-to-video",
        "brand_new_field": true
    }));
    assert_eq!(unknown, vec!["brand_new_field".to_owned()]);
}

#[test]
fn live_fixture_constraint_keys_are_known_to_the_parser() {
    for bytes in live_video_fixtures() {
        let fixture: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        let constraints = &fixture["data"][0]["model_spec"]["constraints"];
        let unknown = unknown_video_constraint_keys(constraints);
        assert!(
            unknown.is_empty(),
            "unknown live video constraint key(s) {unknown:?} — add them to the parser or drop them from the fixture"
        );
    }
}

#[test]
fn swagger_video_constraint_keys_are_known_to_the_parser() {
    for key in SWAGGER_VIDEO_CONSTRAINT_KEYS {
        assert!(
            VIDEO_CONSTRAINT_KEYS.contains(key),
            "swagger Video Model Constraints key {key} is unknown to the parser"
        );
    }
}

#[test]
fn i2v_to_r2v_promotion_requires_the_row_own_reviewed() {
    let overlay = bundled_video_overlay().unwrap();
    for bytes in [IMAGE_TO_VIDEO, SEEDANCE_2_5_R2V] {
        let fixture: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        let id = fixture["data"][0]["id"].as_str().unwrap();
        let live_type = fixture["data"][0]["model_spec"]["constraints"]["model_type"]
            .as_str()
            .unwrap();
        let model = normalize_model_catalog(bytes, fetched_at())
            .unwrap()
            .remove(0);
        if live_type == "image-to-video" && model.operation == MediaOperation::ReferenceToVideo {
            let row = overlay
                .match_info(id)
                .unwrap()
                .unwrap_or_else(|| panic!("promoted {id} has no overlay row"));
            assert!(
                row.own_reviewed,
                "accidental I2V→R2V promotion of {id} via overlay {} without its own reviewed",
                row.key
            );
            assert_eq!(row.operation, Some(MediaOperation::ReferenceToVideo));
        }
    }
}

#[test]
fn t2v_stays_input_constraints_empty_after_overlay() {
    let model = normalize_model_catalog(TEXT_TO_VIDEO, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(model.operation, MediaOperation::TextToVideo);
    assert!(
        model.input_constraints.is_empty(),
        "T2V must stay input_constraints: [] after overlay, got {:?}",
        model
            .input_constraints
            .iter()
            .map(|constraint| constraint.role.as_str().to_owned())
            .collect::<Vec<_>>()
    );
    assert!(
        model
            .input_constraints
            .iter()
            .all(|constraint| !constraint.role.as_str().starts_with("reference")),
        "T2V gained a reference_* role after overlay"
    );
}

#[test]
fn overlay_sources_are_allowlisted() {
    let overlay = bundled_video_overlay().unwrap();
    for row in overlay.rows() {
        let source = row
            .source
            .as_deref()
            .unwrap_or_else(|| panic!("overlay row {} is missing source", row.key));
        assert!(
            ALLOWED_OVERLAY_SOURCES.contains(&source),
            "overlay row {} has unlisted source {source}",
            row.key
        );
    }
}
