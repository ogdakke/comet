//! Provider-neutral Studio cost formatting and draft estimates.

use std::collections::HashMap;

use zeron_proto::StudioTurnView;
use zeron_studio::{MediaModel, ModelId, Quote};

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
    model.estimate_cost(&draft.controls, draft.output_count)
}

pub fn run_quote(
    model: &MediaModel,
    draft: &DraftRunConfig,
    live: &HashMap<ModelId, Quote>,
) -> Option<Quote> {
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
    use zeron_studio::QuoteSource;

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
}
