use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use zeron_studio::{
    ControlId, ControlKind, ControlValue, MediaOperation, ModelFeature, PricingUnit, QuoteSource,
    venice::normalize_model_catalog,
};

const IMAGE: &[u8] = include_bytes!("fixtures/venice/image-model.json");
const TEXT_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/text-to-video-model.json");
const IMAGE_TO_VIDEO: &[u8] = include_bytes!("fixtures/venice/image-to-video-model.json");

fn fetched_at() -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_777_000_000, 0).unwrap()
}

#[test]
fn real_catalog_fixtures_render_as_provider_neutral_controls() {
    let image = normalize_model_catalog(IMAGE, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(image.operation, MediaOperation::TextToImage);
    assert_eq!(image.prompt_maximum_chars, Some(10_000));
    assert_control(&image, "aspect_ratio", ControlKind::AspectRatio, 8);
    assert_control(&image, "resolution", ControlKind::Resolution, 3);
    assert_control(&image, "quality", ControlKind::Enum, 3);
    assert_control(&image, "steps", ControlKind::Integer, 0);
    assert_control(&image, "format", ControlKind::Enum, 3);
    assert_control(&image, "safe_mode", ControlKind::Boolean, 0);
    let safe_mode = image
        .controls
        .iter()
        .find(|control| control.id == ControlId::from("safe_mode"))
        .unwrap();
    assert_eq!(
        safe_mode.default,
        Some(zeron_studio::ControlValue::Boolean { value: false })
    );
    assert_eq!(image.features, vec![ModelFeature::Anon]);

    let text_video = normalize_model_catalog(TEXT_TO_VIDEO, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(text_video.operation, MediaOperation::TextToVideo);
    assert!(text_video.input_constraints.is_empty());
    assert_control(&text_video, "duration", ControlKind::Duration, 9);
    assert_control(&text_video, "audio", ControlKind::Boolean, 0);
    assert_eq!(
        text_video.features,
        vec![ModelFeature::Uncensored, ModelFeature::Anon]
    );

    let image_video = normalize_model_catalog(IMAGE_TO_VIDEO, fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(image_video.operation, MediaOperation::ImageToVideo);
    assert_eq!(image_video.input_constraints.len(), 1);
    assert_eq!(image_video.input_constraints[0].role.as_str(), "source");
    assert!(
        image_video
            .controls
            .iter()
            .all(|control| control.id.as_str() != "aspect_ratio")
    );
}

#[test]
fn operation_and_controls_do_not_depend_on_model_name() {
    let mut fixture: serde_json::Value = serde_json::from_slice(TEXT_TO_VIDEO).unwrap();
    fixture["data"][0]["id"] = "opaque-provider-id".into();
    fixture["data"][0]["model_spec"]["name"] = "Renamed Tomorrow".into();

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(model.operation, MediaOperation::TextToVideo);
    assert_control(&model, "duration", ControlKind::Duration, 9);
}

#[test]
fn auto_aspect_ratio_is_a_first_class_choice() {
    let mut fixture: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    fixture["data"][0]["model_spec"]["constraints"]["aspectRatios"] =
        serde_json::json!(["auto", "1:1", "16:9"]);
    fixture["data"][0]["model_spec"]["constraints"]["defaultAspectRatio"] =
        serde_json::json!("auto");

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    let aspect = model
        .controls
        .iter()
        .find(|control| control.id == ControlId::from("aspect_ratio"))
        .unwrap();
    assert_eq!(aspect.choices.len(), 3);
    assert_eq!(
        aspect.choices[0].value,
        zeron_studio::ControlValue::AspectRatioAuto
    );
    assert_eq!(aspect.choices[0].label, "Auto");
    assert_eq!(
        aspect.default,
        Some(zeron_studio::ControlValue::AspectRatioAuto)
    );
}

#[test]
fn auto_only_aspect_ratio_keeps_the_model() {
    let mut fixture: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    fixture["data"][0]["id"] = "flux-2-max".into();
    fixture["data"][0]["model_spec"]["constraints"]["aspectRatios"] = serde_json::json!(["auto"]);
    fixture["data"][0]["model_spec"]["constraints"]["defaultAspectRatio"] =
        serde_json::json!("auto");

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(model.id.as_str(), "flux-2-max");
    let aspect = model
        .controls
        .iter()
        .find(|control| control.id == ControlId::from("aspect_ratio"))
        .unwrap();
    assert_eq!(aspect.choices.len(), 1);
    assert_eq!(
        aspect.default,
        Some(zeron_studio::ControlValue::AspectRatioAuto)
    );
}

#[test]
fn one_unusable_model_does_not_fail_the_catalog() {
    let mut fixture: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    let good = fixture["data"][0].clone();
    let mut broken = good.clone();
    broken["id"] = "broken-model".into();
    broken["model_spec"]["constraints"]["steps"] = serde_json::json!("nope");
    fixture["data"] = serde_json::json!([broken, good]);

    let models =
        normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at()).unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id.as_str(), "gpt-image-2");
}

#[test]
fn reasoning_is_exposed_only_when_the_model_advertises_it() {
    let mut fixture: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    fixture["data"][0]["model_spec"]["supportsOptimizePromptThinking"] = serde_json::json!(true);

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    let reasoning = model
        .controls
        .iter()
        .find(|control| control.id == ControlId::from("reasoning"))
        .unwrap();
    assert_eq!(reasoning.kind, ControlKind::Boolean);
    assert_eq!(
        reasoning.default,
        Some(zeron_studio::ControlValue::Boolean { value: true })
    );
}

#[test]
fn manifest_version_ignores_display_copy_but_tracks_constraints() {
    let first = normalize_model_catalog(IMAGE, fetched_at())
        .unwrap()
        .remove(0);
    let later = normalize_model_catalog(IMAGE, Utc.timestamp_opt(1_778_000_000, 0).unwrap())
        .unwrap()
        .remove(0);
    assert_eq!(first.manifest_version, later.manifest_version);

    let mut renamed: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    renamed["data"][0]["model_spec"]["name"] = "Seedream 5.0".into();
    renamed["data"][0]["model_spec"]["description"] = "new marketing copy".into();
    renamed["data"][0]["model_spec"]["pricing"] = serde_json::json!({ "usd": 0.02 });
    let renamed = normalize_model_catalog(&serde_json::to_vec(&renamed).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(first.manifest_version, renamed.manifest_version);

    let mut retagged: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    retagged["data"][0]["model_spec"]["privacy"] = "private".into();
    retagged["data"][0]["model_spec"]["uncensored"] = true.into();
    let retagged = normalize_model_catalog(&serde_json::to_vec(&retagged).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_eq!(
        retagged.features,
        vec![ModelFeature::Uncensored, ModelFeature::Private]
    );
    assert_eq!(first.manifest_version, retagged.manifest_version);

    let mut changed: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    changed["data"][0]["model_spec"]["constraints"]["steps"]["max"] = 51.into();
    let changed = normalize_model_catalog(&serde_json::to_vec(&changed).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert_ne!(first.manifest_version, changed.manifest_version);
}

#[test]
fn catalog_quality_matrix_estimates_the_selected_tier() {
    let model = normalize_model_catalog(IMAGE, fetched_at())
        .unwrap()
        .remove(0);
    let pricing = model.pricing.as_ref().expect("fixture advertises pricing");
    assert_eq!(pricing.currency, "USD");
    assert_eq!(pricing.unit, PricingUnit::PerOutput);
    assert!(pricing.amount.is_none());
    assert!(
        pricing
            .entries
            .iter()
            .any(|entry| entry.when.len() == 2 && (entry.amount - 0.26).abs() < f64::EPSILON)
    );

    let high = model
        .estimate_cost(
            &BTreeMap::from([
                (
                    ControlId::from("resolution"),
                    ControlValue::Resolution { value: "1K".into() },
                ),
                (
                    ControlId::from("quality"),
                    ControlValue::Enum {
                        value: "high".into(),
                    },
                ),
            ]),
            1,
        )
        .unwrap();
    assert_eq!(high.source, QuoteSource::Catalog);
    assert_eq!(high.currency, "USD");
    assert!((high.amount - 0.26).abs() < f64::EPSILON);

    let low = model
        .estimate_cost(
            &BTreeMap::from([
                (
                    ControlId::from("resolution"),
                    ControlValue::Resolution { value: "1K".into() },
                ),
                (
                    ControlId::from("quality"),
                    ControlValue::Enum {
                        value: "low".into(),
                    },
                ),
            ]),
            1,
        )
        .unwrap();
    assert!((low.amount - 0.02).abs() < f64::EPSILON);

    let two = model
        .estimate_cost(
            &BTreeMap::from([
                (
                    ControlId::from("resolution"),
                    ControlValue::Resolution { value: "1K".into() },
                ),
                (
                    ControlId::from("quality"),
                    ControlValue::Enum {
                        value: "high".into(),
                    },
                ),
            ]),
            2,
        )
        .unwrap();
    assert!((two.amount - 0.52).abs() < f64::EPSILON);
}

#[test]
fn catalog_defaults_supply_quality_when_the_draft_omits_it() {
    let model = normalize_model_catalog(IMAGE, fetched_at())
        .unwrap()
        .remove(0);
    let quote = model
        .estimate_cost(
            &BTreeMap::from([(
                ControlId::from("resolution"),
                ControlValue::Resolution { value: "1K".into() },
            )]),
            1,
        )
        .unwrap();
    assert!((quote.amount - 0.26).abs() < f64::EPSILON);
}

#[test]
fn resolution_schedule_is_used_when_quality_is_absent() {
    let mut fixture: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    fixture["data"][0]["model_spec"]["pricing"]
        .as_object_mut()
        .unwrap()
        .remove("quality");
    fixture["data"][0]["model_spec"]["constraints"]
        .as_object_mut()
        .unwrap()
        .remove("qualities");
    fixture["data"][0]["model_spec"]["constraints"]
        .as_object_mut()
        .unwrap()
        .remove("defaultQuality");

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert!(
        model
            .controls
            .iter()
            .all(|control| control.id.as_str() != "quality")
    );
    let quote = model
        .estimate_cost(
            &BTreeMap::from([(
                ControlId::from("resolution"),
                ControlValue::Resolution { value: "2K".into() },
            )]),
            1,
        )
        .unwrap();
    assert!((quote.amount - 0.51).abs() < f64::EPSILON);
}

#[test]
fn flat_generation_price_is_normalized() {
    let mut fixture: serde_json::Value = serde_json::from_slice(IMAGE).unwrap();
    fixture["data"][0]["model_spec"]["pricing"] = serde_json::json!({
        "generation": { "usd": 0.01, "diem": 0.01 }
    });

    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    let quote = model.estimate_cost(&BTreeMap::new(), 3).unwrap();
    assert!((quote.amount - 0.03).abs() < f64::EPSILON);
}

#[test]
fn missing_pricing_yields_no_estimate() {
    let mut fixture: serde_json::Value = serde_json::from_slice(TEXT_TO_VIDEO).unwrap();
    fixture["data"][0]["model_spec"]
        .as_object_mut()
        .unwrap()
        .remove("pricing");
    let model = normalize_model_catalog(&serde_json::to_vec(&fixture).unwrap(), fetched_at())
        .unwrap()
        .remove(0);
    assert!(model.pricing.is_none());
    assert!(model.estimate_cost(&BTreeMap::new(), 1).is_none());
}

fn assert_control(
    model: &zeron_studio::MediaModel,
    id: &str,
    kind: ControlKind,
    choice_count: usize,
) {
    let control = model
        .controls
        .iter()
        .find(|control| control.id == ControlId::from(id))
        .unwrap();
    assert_eq!(control.kind, kind);
    assert_eq!(control.choices.len(), choice_count);
}
