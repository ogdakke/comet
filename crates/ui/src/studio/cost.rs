//! Provider-neutral Studio cost formatting and draft estimates.

use std::collections::HashMap;

use zeron_proto::StudioTurnView;
use zeron_studio::{MediaKind, MediaModel, ModelId, Quote};

use super::draft::DraftRunConfig;

pub fn format_quote(quote: &Quote) -> String {
    format_amount(&quote.currency, quote.amount)
}

pub fn format_amount(currency: &str, amount: f64) -> String {
    let symbol = match currency {
        "USD" | "usd" => "$",
        other => return format!("{amount} {other}"),
    };
    if amount == 0.0 {
        return format!("{symbol}0");
    }
    if amount.abs() >= 0.01 {
        format!("{symbol}{amount:.2}")
    } else {
        let digits = format!("{amount:.4}");
        let digits = digits.trim_end_matches('0').trim_end_matches('.');
        format!("{symbol}{digits}")
    }
}

pub fn estimate_draft_quote(model: &MediaModel, draft: &DraftRunConfig) -> Option<Quote> {
    // Quote is the video price; catalog estimates are image-only.
    if model.output_kind == MediaKind::Video {
        return None;
    }
    model.estimate_cost(&draft.controls, draft.output_count)
}

pub fn run_quote(
    model: &MediaModel,
    draft: &DraftRunConfig,
    live: &HashMap<ModelId, Quote>,
) -> Option<Quote> {
    if model.output_kind == MediaKind::Video {
        return live.get(&model.id).cloned();
    }
    estimate_draft_quote(model, draft).or_else(|| live.get(&model.id).cloned())
}

pub fn selected_batch_quote(
    models: &[MediaModel],
    selected: &std::collections::BTreeSet<ModelId>,
    drafts: &HashMap<ModelId, DraftRunConfig>,
    live: &HashMap<ModelId, Quote>,
) -> Option<Quote> {
    let quotes = models
        .iter()
        .filter(|model| selected.contains(&model.id))
        .map(|model| {
            let draft = drafts
                .get(&model.id)
                .cloned()
                .unwrap_or_else(|| DraftRunConfig::from_model(model));
            run_quote(model, &draft, live)
        })
        .collect::<Option<Vec<_>>>()?;
    if quotes.is_empty() {
        return None;
    }
    Quote::total(quotes)
}

pub fn needs_live_quote(
    models: &[MediaModel],
    selected: &std::collections::BTreeSet<ModelId>,
    drafts: &HashMap<ModelId, DraftRunConfig>,
) -> bool {
    models
        .iter()
        .filter(|model| selected.contains(&model.id))
        .any(|model| {
            if model.output_kind == MediaKind::Video {
                return true;
            }
            let draft = drafts
                .get(&model.id)
                .cloned()
                .unwrap_or_else(|| DraftRunConfig::from_model(model));
            estimate_draft_quote(model, &draft).is_none()
        })
}

pub fn remaining_after_spend(balance: &Quote, spend: Option<&Quote>) -> Quote {
    spend
        .and_then(|spend| balance.saturating_sub(spend))
        .unwrap_or_else(|| balance.clone())
}

pub fn turn_quote(turn: &StudioTurnView) -> Option<Quote> {
    let quotes = turn
        .runs
        .iter()
        .map(|run| run.quote.clone())
        .collect::<Option<Vec<_>>>()?;
    if quotes.is_empty() {
        return None;
    }
    Quote::total(quotes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeSet, HashMap};
    use zeron_studio::{
        AdapterFamily, AudioCapability, MediaKind, MediaOperation, ModelId, PricingMetadata,
        PricingUnit, QuoteSource, VideoModelMeta,
    };

    fn video_model_with_catalog_price(id: &str, amount: f64) -> MediaModel {
        MediaModel {
            provider_id: "venice".into(),
            id: ModelId::new(id),
            display_name: id.into(),
            description: None,
            operation: MediaOperation::TextToVideo,
            output_kind: MediaKind::Video,
            output_mime_types: vec!["video/mp4".into()],
            input_constraints: Vec::new(),
            prompt_maximum_chars: None,
            negative_prompt_maximum_chars: None,
            maximum_output_count: 1,
            controls: Vec::new(),
            pricing: Some(PricingMetadata {
                currency: "USD".into(),
                unit: PricingUnit::PerOutput,
                unit_label: String::new(),
                amount: Some(amount),
                entries: Vec::new(),
                detail: None,
            }),
            features: Vec::new(),
            video: VideoModelMeta {
                adapter_family: AdapterFamily::Seedance,
                generate_audio: AudioCapability::Configurable { default: true },
                ..VideoModelMeta::default()
            },
            manifest_version: "test".into(),
            fetched_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn usd_amounts_keep_cents_until_they_need_more() {
        assert_eq!(format_amount("USD", 0.27), "$0.27");
        assert_eq!(format_amount("USD", 0.52), "$0.52");
        assert_eq!(format_amount("USD", 0.0035), "$0.0035");
        assert_eq!(format_amount("USD", 0.0), "$0");
    }

    #[test]
    fn remaining_after_spend_clamps_at_zero() {
        let remaining = remaining_after_spend(
            &Quote::catalog("USD", 0.40),
            Some(&Quote::catalog("USD", 0.67)),
        );
        assert_eq!(format_quote(&remaining), "$0");
    }

    #[test]
    fn remaining_after_spend_keeps_the_confirmed_currency() {
        let remaining = remaining_after_spend(
            &Quote::catalog("USD", 12.34),
            Some(&Quote::catalog("USD", 0.34)),
        );
        assert_eq!(format_quote(&remaining), "$12.00");
    }

    #[test]
    fn totals_require_every_run_and_one_currency() {
        let first = Quote::catalog("USD", 0.26);
        let second = Quote::catalog("USD", 0.02);
        let total = Quote::total([first, second]).unwrap();
        assert_eq!(total.source, QuoteSource::Catalog);
        assert!((total.amount - 0.28).abs() < f64::EPSILON);
        assert!(Quote::total([Quote::catalog("USD", 0.1), Quote::catalog("EUR", 0.1)]).is_none());
    }

    #[test]
    fn video_uses_live_quote_not_catalog_estimate() {
        let model = video_model_with_catalog_price("seedance-t2v", 0.99);
        let draft = DraftRunConfig::from_model(&model);
        assert!(estimate_draft_quote(&model, &draft).is_none());

        let selected = BTreeSet::from([model.id.clone()]);
        let drafts = HashMap::from([(model.id.clone(), draft.clone())]);
        assert!(needs_live_quote(&[model.clone()], &selected, &drafts));

        let live = HashMap::from([(model.id.clone(), Quote::provider("USD", 1.25))]);
        let quoted = run_quote(&model, &draft, &live).unwrap();
        assert_eq!(quoted.source, QuoteSource::Provider);
        assert!((quoted.amount - 1.25).abs() < f64::EPSILON);
        assert!(run_quote(&model, &draft, &HashMap::new()).is_none());
    }
}
